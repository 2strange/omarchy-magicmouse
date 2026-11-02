import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Magic Mouse settings popup. Every control writes straight to
// ~/.config/magic-mouse/config.toml through `magic-mouse-config`; the daemon
// hot-reloads that file, so sliders take effect while you drag.
Panel {
  id: root
  moduleName: "io.github.2strange.magicmouse"
  ipcTarget: "io.github.2strange.magicmouse"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root
  property bool openedFromHotkey: false
  property string tab: "point"  // Pointer / Scroll / Gestures / Touch

  // Battery info handed over by the bar widget.
  property string batteryText: ""

  function open() {
    openedFromHotkey = false
    setCenterHoverRevealSuppressed(false)
    root.controller.show()
    root.snapshotPending = true
    root.refresh()
  }
  function openFromHotkey() {
    openedFromHotkey = true
    root.controller.show()
    root.snapshotPending = true
    root.refresh()
    Qt.callLater(function() { if (root.opened) setCenterHoverRevealSuppressed(true) })
  }
  // Hide first: if anything after this throws, the full-screen overlay must
  // still go away, or it keeps eating every click on every monitor.
  function close() {
    root.controller.hide()
    setCenterHoverRevealSuppressed(false)
  }
  function toggle() {
    if (root.opened) root.close()
    else root.openFromHotkey()
  }
  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }
  // On Omarchy 4.0.3+ third-party widgets get PluginBarApi, where
  // centerHoverRevealSuppressed is read-only and assigning it throws; go
  // through its setter. On older Omarchy `bar` is the raw Bar, which has the
  // writable property but no setter, so fall back to assigning it directly
  // (same shape as Omarchy's own weather/clock panels).
  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && typeof root.bar.setCenterHoverRevealSuppressed === "function")
      root.bar.setCenterHoverRevealSuppressed(value)
    else if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }

  // ---- config state
  property var cfg: null
  property bool snapshotPending: false
  function val(section, key, fallback) {
    if (!cfg || !cfg[section] || cfg[section][key] === undefined) return fallback
    return cfg[section][key]
  }
  function refresh() {
    if (!readProc.running) readProc.running = true
  }

  Process {
    id: readProc
    command: ["magic-mouse-config", "get"]
    stdout: StdioCollector {
      onStreamFinished: {
        try { root.cfg = JSON.parse(text) } catch (e) { console.warn("magic-mouse: bad config json", e) }
        // Snapshot for Undo only from a file with no write in flight, or it would be stale.
        if (root.snapshotPending && root.cfg && !writeTimer.running && !writeProc.running) {
          root.openedCfg = JSON.parse(JSON.stringify(root.cfg))
          root.snapshotPending = false
        }
      }
    }
  }

  // Pending writes are coalesced so a slider drag becomes a handful of writes.
  property var pending: ({})
  function set(section, key, value) {
    var next = Object.assign({}, pending)
    next[section + "." + key] = value
    pending = next
    // Optimistic local update so the UI doesn't snap back before the reload.
    if (cfg && cfg[section]) {
      var c = JSON.parse(JSON.stringify(cfg))
      c[section][key] = value
      cfg = c
    }
    writeTimer.restart()
  }
  Timer {
    id: writeTimer
    interval: 120
    onTriggered: root.flush()
  }
  function flush() {
    var keys = Object.keys(pending)
    if (keys.length === 0 || writeProc.running) { if (keys.length) writeTimer.restart(); return }
    var args = ["magic-mouse-config", "set"]
    for (var i = 0; i < keys.length; i++) args.push(keys[i] + "=" + String(pending[keys[i]]))
    pending = ({})
    writeProc.command = args
    writeProc.running = true
  }
  Process {
    id: writeProc
    onExited: function(code) {
      if (code !== 0) console.warn("magic-mouse-config failed", code)
      if (root.snapshotPending) root.refresh()
    }
  }

  FileView {
    path: Quickshell.env("HOME") + "/.config/magic-mouse/config.toml"
    watchChanges: true
    printErrors: false
    onFileChanged: if (!writeTimer.running && !writeProc.running) root.refresh()
  }
  Component.onCompleted: root.refresh()

  readonly property color fg: bar ? bar.foreground : Color.foreground
  readonly property color dim: Qt.darker(fg, 1.4)
  readonly property string fontFam: bar ? bar.fontFamily : Style.font.family

  function fmt1(v) { return (Math.round(v * 100) / 100).toString() }
  function mmText(v) { return v <= 0 ? "off" : fmt1(v) + " mm" }

  // ---- undo / defaults
  // The keys each tab shows; "Defaults" resets the open tab, "Undo" restores every
  // key to what it was when the menu opened.
  readonly property var tabKeys: ({
    point: ["pointer.tracking_speed", "buttons.right_click", "buttons.control_click", "bar.show_percent"],
    scroll: ["touchpad.enabled", "touchpad.scroll_speed", "scroll.natural", "scroll.speed", "scroll.start_mm", "scroll.free_axis", "momentum.enabled", "momentum.decay"],
    gestures: ["touchpad.swipe", "touchpad.swipe_reverse", "touchpad.swipe_gain", "gestures.swipe_pages", "gestures.two_finger_swipe_left", "gestures.two_finger_swipe_right", "gestures.invert_swipe", "gestures.swipe_mm", "gestures.swipe_end", "gestures.include_empty"],
    touch: ["scroll.edge_front_mm", "scroll.edge_left_mm", "scroll.edge_right_mm", "scroll.edge_back_mm", "scroll.edge_release_mm"]
  })
  property var defaults: null
  readonly property bool tp: val("touchpad", "enabled", false) === true  // touchpad mode
  property var openedCfg: null  // snapshot when the menu opened, for Undo
  function keyVal(src, path) {
    var p = path.split(".")
    return src && src[p[0]] ? src[p[0]][p[1]] : undefined
  }
  function applyFrom(src, keys) {
    for (var i = 0; i < keys.length; i++) {
      var v = keyVal(src, keys[i])
      if (v === undefined || v === keyVal(cfg, keys[i])) continue
      var p = keys[i].split(".")
      set(p[0], p[1], v)
    }
  }
  function resetTab() { if (defaults) applyFrom(defaults, tabKeys[tab]) }
  function undoAll() {
    if (!openedCfg) return
    var all = []
    for (var t in tabKeys) all = all.concat(tabKeys[t])
    applyFrom(openedCfg, all)
  }
  readonly property bool changed: {
    if (!openedCfg || !cfg) return false
    for (var t in tabKeys)
      for (var i = 0; i < tabKeys[t].length; i++)
        if (keyVal(openedCfg, tabKeys[t][i]) !== keyVal(cfg, tabKeys[t][i])) return true
    return false
  }
  Process {
    id: defaultsProc
    command: ["magic-mouse-config", "defaults"]
    running: true
    stdout: StdioCollector {
      onStreamFinished: { try { root.defaults = JSON.parse(text) } catch (e) { console.warn("magic-mouse: bad defaults json", e) } }
    }
  }

  // ---- a labelled slider row
  component SliderRow: Column {
    id: row
    property string label: ""
    property string valueText: ""
    property real minimum: 0
    property real maximum: 1
    property real step: 0.05
    property real value: 0
    signal changed(real v)
    width: parent.width
    spacing: Style.space(4)
    Item {
      width: parent.width
      height: labelText.implicitHeight
      Text { textFormat: Text.PlainText; id: labelText; text: row.label; color: root.fg; font.family: root.fontFam; font.pixelSize: Style.font.body; anchors.left: parent.left }
      Text { textFormat: Text.PlainText; text: row.valueText; color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; anchors.right: parent.right; anchors.verticalCenter: parent.verticalCenter }
    }
    function snap(v) {
      var s = Math.round((v - row.minimum) / row.step) * row.step + row.minimum
      s = Math.max(row.minimum, Math.min(row.maximum, s))
      return Math.round(s * 10000) / 10000
    }
    PanelSlider {
      id: slider
      bar: root.bar
      width: parent.width
      minimum: row.minimum
      maximum: row.maximum
      step: row.step
      value: row.value
      onMoved: function(v) { row.changed(row.snap(v)) }
      onReleased: function(v) { row.changed(row.snap(v)) }
      // The Magic Mouse scrolls at the lightest touch; a slider that follows the
      // wheel would change while you only move over it. Drag or click only.
      MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.NoButton
        onWheel: function(wheel) { wheel.accepted = true }
      }
    }
  }

  // ---- a labelled switch row
  component SwitchRow: Item {
    id: srow
    property string label: ""
    property string description: ""
    property bool checked: false
    signal toggled()
    width: parent.width
    implicitHeight: Math.max(labels.implicitHeight, sw.implicitHeight)
    Column {
      id: labels
      anchors.left: parent.left
      anchors.right: sw.left
      anchors.rightMargin: Style.space(12)
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(2)
      Text { textFormat: Text.PlainText; text: srow.label; color: root.fg; font.family: root.fontFam; font.pixelSize: Style.font.body; width: parent.width; elide: Text.ElideRight }
      Text { textFormat: Text.PlainText; text: srow.description; visible: text !== ""; color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; width: parent.width; wrapMode: Text.WordWrap }
    }
    ToggleSwitch {
      id: sw
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      checked: srow.checked
      foreground: root.fg
      onToggled: srow.toggled()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }

      Column {
        id: column
        anchors.fill: parent
        spacing: Style.space(14)

        // ---------- Hero ----------
        Item {
          width: parent.width
          implicitHeight: Math.max(heroIcon.implicitHeight, heroLabels.implicitHeight)
          Text {
            textFormat: Text.PlainText
            id: heroIcon
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            text: "󰍽"
            color: root.fg
            font.family: root.fontFam
            font.pixelSize: Style.font.display
          }
          Column {
            id: heroLabels
            anchors.left: heroIcon.right
            anchors.leftMargin: Style.space(14)
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)
            Text { textFormat: Text.PlainText; text: "MagicMouse"; color: root.fg; font.family: root.fontFam; font.pixelSize: Style.font.title; font.bold: true; width: parent.width; elide: Text.ElideRight }
            Repeater {  // one line per connected mouse
              model: (root.batteryText !== "" ? root.batteryText : "MAC-LIKE ON OMARCHY").split("\n")
              Text { required property var modelData; textFormat: Text.PlainText; text: modelData; color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; font.bold: true; width: parent.width; elide: Text.ElideRight }
            }
          }
        }

        PanelSeparator { width: parent.width; foreground: root.fg }

        ButtonGroup {
          width: parent.width
          options: [
            { value: "point", label: "Pointer" },
            { value: "scroll", label: "Scroll" },
            { value: "gestures", label: "Gestures" },
            { value: "touch", label: "Touch" }
          ]
          value: root.tab
          foreground: root.fg
          fontFamily: root.fontFam
          fontSize: Style.font.caption
          focusable: false
          onChanged: function(v) { root.tab = v }
        }

        Column {
          width: parent.width
          spacing: Style.space(14)
          visible: root.tab === "point"
          // ---------- Pointer ----------
          PanelSectionHeader { text: "POINTER"; foreground: root.fg; fontFamily: root.fontFam }
          SliderRow {
            label: "Tracking speed"
            valueText: root.fmt1(root.val("pointer", "tracking_speed", 0.6875)) + (Math.abs(root.val("pointer", "tracking_speed", 0.6875) - 0.6875) < 0.01 ? "  (Apple default)" : "")
            minimum: 0; maximum: 3; step: 0.0625
            value: root.val("pointer", "tracking_speed", 0.6875)
            onChanged: function(v) { root.set("pointer", "tracking_speed", Math.round(v * 10000) / 10000) }
          }

          // ---------- Clicking ----------
          PanelSectionHeader { text: "CLICKING"; foreground: root.fg; fontFamily: root.fontFam }
          SwitchRow {
            label: "Right-side click is a right click"
            description: "Off: the whole mouse left-clicks, like macOS with secondary click off"
            checked: root.val("buttons", "right_click", true) === true
            onToggled: root.set("buttons", "right_click", !(root.val("buttons", "right_click", true) === true))
          }
          SwitchRow {
            label: "Control-click is a right click"
            description: "Hold Control and click to open menus, like on a Mac"
            checked: root.val("buttons", "control_click", false) === true
            onToggled: root.set("buttons", "control_click", !(root.val("buttons", "control_click", false) === true))
          }

          // ---------- Bar ----------
          PanelSectionHeader { text: "BAR"; foreground: root.fg; fontFamily: root.fontFam }
          SwitchRow {
            label: "Show battery percentage"
            description: "Off shows just the icon; low battery still highlights"
            checked: root.val("bar", "show_percent", true) === true
            onToggled: root.set("bar", "show_percent", !(root.val("bar", "show_percent", true) === true))
          }
        }

        Column {
          width: parent.width
          spacing: Style.space(14)
          visible: root.tab === "scroll"
          // ---------- Scrolling ----------
          PanelSectionHeader { text: "SCROLLING"; foreground: root.fg; fontFamily: root.fontFam }
          SwitchRow {
            label: "Touchpad mode"
            description: root.tp
              ? "The mouse scrolls and swipes like a trackpad: apps scroll and glide themselves, the workspace follows your fingers"
              : "Let the mouse act as a trackpad (one finger more): smoother scrolling and the live workspace swipe"
            checked: root.tp
            onToggled: root.set("touchpad", "enabled", !root.tp)
          }
          SwitchRow {
            label: "Natural scrolling"
            description: "Content follows the finger"
            checked: root.val("scroll", "natural", true) === true
            onToggled: root.set("scroll", "natural", !(root.val("scroll", "natural", true) === true))
          }
          SliderRow {
            visible: root.tp
            label: "Scroll speed"
            valueText: (Math.round(root.val("touchpad", "scroll_speed", 0.35) * 1000) / 1000).toString() + "×"
            minimum: 0.05; maximum: 2.0; step: 0.025
            value: root.val("touchpad", "scroll_speed", 0.35)
            onChanged: function(v) { root.set("touchpad", "scroll_speed", Math.round(v * 1000) / 1000) }
          }
          SliderRow {
            visible: !root.tp
            label: "Scroll speed"
            valueText: root.fmt1(root.val("scroll", "speed", 1.0)) + "×"
            minimum: 0.4; maximum: 2.5; step: 0.1
            value: root.val("scroll", "speed", 1.0)
            onChanged: function(v) { root.set("scroll", "speed", Math.round(v * 100) / 100) }
          }
          SliderRow {
            visible: !root.tp
            label: "Finger travel before a scroll starts"
            valueText: root.fmt1(root.val("scroll", "start_mm", 0.8)) + " mm"
            minimum: 0.3; maximum: 2.0; step: 0.1
            value: root.val("scroll", "start_mm", 0.8)
            onChanged: function(v) { root.set("scroll", "start_mm", Math.round(v * 100) / 100) }
          }
          SwitchRow {
            visible: !root.tp
            label: "Diagonal scrolling"
            description: "Scroll up/down and sideways at once, like in a code editor. Off: each scroll locks to one direction"
            checked: root.val("scroll", "free_axis", false) === true
            onToggled: root.set("scroll", "free_axis", !(root.val("scroll", "free_axis", false) === true))
          }

          // ---------- Momentum ----------
          PanelSectionHeader { visible: !root.tp; text: "MOMENTUM"; foreground: root.fg; fontFamily: root.fontFam }
          SwitchRow {
            visible: !root.tp
            label: "Momentum scrolling"
            description: "Flick and lift, the page keeps gliding"
            checked: root.val("momentum", "enabled", true) === true
            onToggled: root.set("momentum", "enabled", !(root.val("momentum", "enabled", true) === true))
          }
          SliderRow {
            visible: !root.tp
            label: "Glide length"
            valueText: root.val("momentum", "decay", 0.97) >= 0.983 ? "long (macOS)" : (root.val("momentum", "decay", 0.97) <= 0.955 ? "short" : "medium")
            minimum: 0.94; maximum: 0.99; step: 0.005
            value: root.val("momentum", "decay", 0.97)
            onChanged: function(v) { root.set("momentum", "decay", Math.round(v * 1000) / 1000) }
          }
        }

        Column {
          width: parent.width
          spacing: Style.space(14)
          visible: root.tab === "gestures"
          // ---------- Gestures ----------
          PanelSectionHeader { text: "GESTURES"; foreground: root.fg; fontFamily: root.fontFam }
          Text {
            textFormat: Text.PlainText
            visible: root.tp
            width: parent.width
            text: "Touchpad mode: two fingers sideways switch workspaces like three fingers on the trackpad (live, or at the end of the swipe for Wrap around). End of row, empty workspaces and the trackpad's direction come from the Mac Trackpad settings (Fingers: 3)."
            color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; wrapMode: Text.WordWrap
          }
          SwitchRow {
            visible: root.tp
            label: "Two-finger workspace swipe"
            description: "Off: two fingers sideways just scroll"
            checked: root.val("touchpad", "swipe", true) === true
            onToggled: root.set("touchpad", "swipe", !(root.val("touchpad", "swipe", true) === true))
          }
          SwitchRow {
            visible: root.tp
            label: "Reverse swipe direction"
            description: "The mouse swipes the other way round than the trackpad"
            checked: root.val("touchpad", "swipe_reverse", false) === true
            onToggled: root.set("touchpad", "swipe_reverse", !(root.val("touchpad", "swipe_reverse", false) === true))
          }
          SliderRow {
            visible: root.tp
            label: "Swipe boost"
            valueText: root.fmt1(root.val("touchpad", "swipe_gain", 2.7)) + "×"
            minimum: 1.0; maximum: 5.0; step: 0.1
            value: root.val("touchpad", "swipe_gain", 2.7)
            onChanged: function(v) { root.set("touchpad", "swipe_gain", Math.round(v * 10) / 10) }
          }
          Text {
            textFormat: Text.PlainText
            visible: root.tp
            width: parent.width
            text: "The mouse is small: finger travel counts this many times during a swipe"
            color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; wrapMode: Text.WordWrap
          }
          SwitchRow {
            visible: !root.tp
            label: "Swipe between pages"
            description: "One-finger sideways flick = back / forward"
            checked: root.val("gestures", "swipe_pages", true) === true
            onToggled: root.set("gestures", "swipe_pages", !(root.val("gestures", "swipe_pages", true) === true))
          }
          SwitchRow {
            visible: !root.tp
            label: "Two-finger swipe switches workspace"
            checked: String(root.val("gestures", "two_finger_swipe_left", "")) !== ""
            onToggled: {
              var on = String(root.val("gestures", "two_finger_swipe_left", "")) !== ""
              root.set("gestures", "two_finger_swipe_left", on ? "" : 'dispatch hl.dsp.focus({ workspace = "e+1" })')
              root.set("gestures", "two_finger_swipe_right", on ? "" : 'dispatch hl.dsp.focus({ workspace = "e-1" })')
            }
          }
          Text {
            textFormat: Text.PlainText
            visible: !root.tp
            width: parent.width
            text: "Swipe distance applies to the two-finger swipe and the one-finger page flick"
            color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; wrapMode: Text.WordWrap
          }
          SliderRow {
            visible: !root.tp
            label: "Swipe distance"
            valueText: root.fmt1(root.val("gestures", "swipe_mm", 8.0)) + " mm"
            minimum: 4; maximum: 16; step: 0.5
            value: root.val("gestures", "swipe_mm", 8.0)
            onChanged: function(v) { root.set("gestures", "swipe_mm", v) }
          }
          Column {
            width: parent.width
            spacing: Style.space(6)
            // only the default workspace actions know about an end; custom commands run as they are
            visible: !root.tp && String(root.val("gestures", "two_finger_swipe_left", "")).indexOf('workspace = "e') !== -1
            Text { textFormat: Text.PlainText; text: "At the last workspace"; color: root.fg; font.family: root.fontFam; font.pixelSize: Style.font.body }
            ButtonGroup {
              width: parent.width
              options: [
                { value: "stop", label: "Stop" },
                { value: "new", label: "New one" },
                { value: "wrap", label: "Wrap around" }
              ]
              value: String(root.val("gestures", "swipe_end", "wrap"))
              foreground: root.fg
              fontFamily: root.fontFam
              fontSize: Style.font.caption
              focusable: false
              onChanged: function(v) { root.set("gestures", "swipe_end", v) }
            }
          }
          SwitchRow {
            visible: !root.tp && String(root.val("gestures", "two_finger_swipe_left", "")).indexOf('workspace = "e') !== -1
            label: "Include empty workspaces"
            description: "Workspaces 1-5 always count, like in the bar. Off: only open ones"
            checked: root.val("gestures", "include_empty", true) === true
            onToggled: root.set("gestures", "include_empty", !(root.val("gestures", "include_empty", true) === true))
          }
          SwitchRow {
            visible: !root.tp
            label: "Invert swipe direction"
            description: "Swap which way the two-finger swipe moves through workspaces"
            checked: root.val("gestures", "invert_swipe", false) === true
            onToggled: root.set("gestures", "invert_swipe", !(root.val("gestures", "invert_swipe", false) === true))
          }
        }

        Column {
          width: parent.width
          spacing: Style.space(14)
          visible: root.tab === "touch"
          // ---------- Touch surface ----------
          PanelSectionHeader { text: "TOUCH SURFACE"; foreground: root.fg; fontFamily: root.fontFam }
          Item {
            width: parent.width
            implicitHeight: Math.max(previewLabels.implicitHeight, previewButton.implicitHeight)
            Column {
              id: previewLabels
              anchors.left: parent.left
              anchors.right: previewButton.left
              anchors.rightMargin: Style.space(12)
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(2)
              Text { textFormat: Text.PlainText; text: "Touch preview"; color: root.fg; font.family: root.fontFam; font.pixelSize: Style.font.body; width: parent.width; elide: Text.ElideRight }
              Text { textFormat: Text.PlainText; text: "Live view of every contact, its size and why the daemon scrolls or not"; color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; width: parent.width; wrapMode: Text.WordWrap }
            }
            PanelActionButton {
              id: previewButton
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              iconText: "󰍽"
              tooltipText: "Open the touch preview window"
              foreground: root.fg
              fontFamily: root.fontFam
              bordered: true
              onClicked: {
                Quickshell.execDetached(["qs", "-p", decodeURIComponent(Qt.resolvedUrl("preview").toString().replace(/^file:\/\//, ""))])
                root.close()
              }
            }
          }
          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: "A touch that starts in an edge zone holds the mouse: it scrolls only after sliding straight on for a while, so a finger resting there is ignored. In a sideways two-finger swipe it always counts. The back zone is the palm and is always ignored."
            color: root.dim; font.family: root.fontFam; font.pixelSize: Style.font.caption; wrapMode: Text.WordWrap
          }
          SliderRow {
            label: "Front edge"
            valueText: root.mmText(root.val("scroll", "edge_front_mm", 2.0))
            minimum: 0; maximum: 20; step: 0.5
            value: root.val("scroll", "edge_front_mm", 2.0)
            onChanged: function(v) { root.set("scroll", "edge_front_mm", v) }
          }
          SliderRow {
            label: "Left edge"
            valueText: root.mmText(root.val("scroll", "edge_left_mm", 3.0))
            minimum: 0; maximum: 25; step: 0.5
            value: root.val("scroll", "edge_left_mm", 3.0)
            onChanged: function(v) { root.set("scroll", "edge_left_mm", v) }
          }
          SliderRow {
            label: "Right edge"
            valueText: root.mmText(root.val("scroll", "edge_right_mm", 3.0))
            minimum: 0; maximum: 25; step: 0.5
            value: root.val("scroll", "edge_right_mm", 3.0)
            onChanged: function(v) { root.set("scroll", "edge_right_mm", v) }
          }
          SliderRow {
            label: "Back (palm, ignored)"
            valueText: root.mmText(root.val("scroll", "edge_back_mm", 18.0))
            minimum: 0; maximum: 40; step: 1
            value: root.val("scroll", "edge_back_mm", 18.0)
            onChanged: function(v) { root.set("scroll", "edge_back_mm", v) }
          }
          SliderRow {
            label: "Edge finger scrolls after"
            valueText: root.mmText(root.val("scroll", "edge_release_mm", 3.0))
            minimum: 1; maximum: 8; step: 0.5
            value: root.val("scroll", "edge_release_mm", 3.0)
            onChanged: function(v) { root.set("scroll", "edge_release_mm", v) }
          }

          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: "More knobs: ~/.config/magic-mouse/config.toml"
            color: root.dim
            font.family: root.fontFam
            font.pixelSize: Style.font.caption
            elide: Text.ElideRight
          }
        }

        PanelSeparator { width: parent.width; foreground: root.fg }
        Item {
          width: parent.width
          implicitHeight: undoButton.implicitHeight
          Button {
            id: undoButton
            anchors.left: parent.left
            text: "Undo changes"
            tooltipText: "Put every setting back to how it was when this menu opened"
            bordered: true
            enabled: root.changed
            opacity: enabled ? 1 : 0.4
            foreground: root.fg
            fontFamily: root.fontFam
            fontSize: Style.font.caption
            onClicked: root.undoAll()
          }
          Button {
            anchors.right: parent.right
            text: "Defaults"
            tooltipText: "Reset the settings on this tab to the built-in defaults"
            bordered: true
            enabled: root.defaults !== null
            foreground: root.fg
            fontFamily: root.fontFam
            fontSize: Style.font.caption
            onClicked: root.resetTab()
          }
        }
      }
    }
  }
}
