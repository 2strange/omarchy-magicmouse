import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.UPower
import qs.Ui

// Mouse battery pill: shows the charge of the first UPower device that is a
// mouse (or, failing that, the first HID battery). Hidden when there is none.
// Settings on the bar entry in shell.json:
//   "match": "MMM"   only consider devices whose model contains this text
//   "lowAt": 20      highlight (amber) at or below this percentage
//   "criticalAt": 10 red at or below this percentage
BarWidget {
  id: root
  moduleName: "io.github.2strange.magicmouse"

  readonly property string match: String(setting("match", ""))
  readonly property int lowAt: Number(setting("lowAt", 20))
  readonly property int criticalAt: Number(setting("criticalAt", 10))
  readonly property var devices: UPower.devices ? UPower.devices.values : []

  readonly property var device: {
    var list = devices
    var i
    if (match !== "") {
      for (i = 0; i < list.length; i++) {
        if (list[i] && String(list[i].model).indexOf(match) >= 0) return list[i]
      }
      return null
    }
    for (i = 0; i < list.length; i++) {
      if (list[i] && list[i].type === UPowerDeviceType.Mouse) return list[i]
    }
    for (i = 0; i < list.length; i++) {
      if (list[i] && String(list[i].nativePath).indexOf("hid-") === 0) return list[i]
    }
    return null
  }

  // Quickshell exposes UPower's percentage as a 0..1 fraction.
  // The magic-mouse daemon asks the mouse directly and publishes the answer
  // here; UPower only learns the level when the mouse volunteers it.
  //
  // The shell never opens that file itself: it lives in a runtime directory
  // other software can write to, and a FIFO or huge file put in its place would
  // stall or exhaust the shell. Instead a short-lived child (status-reader.py)
  // does the opening and prints the status only if the directory is ours and not
  // group/other-writable and the file is a regular, user-owned file of at most
  // 4 KiB; otherwise it prints nothing. The child is asynchronous, so even a
  // wedged one cannot block the shell's event loop; `timeout` reaps it, and only
  // one runs at a time. Its output is length-checked again below. It runs on
  // start, every 60 s (the daemon polls the battery at most once a minute), and
  // whenever the status directory changes; that watch is on the directory only
  // and never loads anything.
  property var status: null
  readonly property string runtimeDir: Quickshell.env("XDG_RUNTIME_DIR") || ""
  readonly property string readerScript: decodeURIComponent(Qt.resolvedUrl("status-reader.py").toString().replace(/^file:\/\//, ""))
  readonly property int maxStatusChars: 4096
  Process {
    id: statusProc
    command: ["timeout", "-k", "1", "3", "python3", "-I", root.readerScript, root.runtimeDir + "/magic-mouse/battery.json"]
    stdout: StdioCollector {
      onStreamFinished: root.parseStatus(text)
    }
    onExited: function(code) { if (code !== 0) root.status = null }
  }
  function refreshStatus() { if (root.runtimeDir !== "" && !statusProc.running) statusProc.running = true }
  Timer {
    interval: 60000
    repeat: true
    running: root.runtimeDir !== ""
    triggeredOnStart: true
    onTriggered: root.refreshStatus()
  }
  FileView {
    path: root.runtimeDir !== "" ? root.runtimeDir + "/magic-mouse" : ""
    preload: false
    watchChanges: true
    onFileChanged: root.refreshStatus()
  }
  function parseStatus(content) {
    try {
      var raw = String(content || "")
      if (raw.length === 0 || raw.length > root.maxStatusChars) {
        root.status = null
        return
      }
      var parsed = JSON.parse(raw)
      root.status = parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : null
    } catch (e) {
      root.status = null
    }
  }

  readonly property bool daemonHas: status !== null && status.connected === true && status.percent !== undefined
  readonly property bool upowerKnown: device !== null && !(Math.round(device.percentage * 100) === 0 && device.state === UPowerDeviceState.Unknown)
  readonly property bool connected: daemonHas || (status !== null && status.connected === true) || device !== null
  readonly property bool known: daemonHas || upowerKnown
  readonly property int percent: daemonHas ? Number(status.percent) : (device ? Math.round(device.percentage * 100) : -1)
  readonly property bool charging: daemonHas ? status.charging === true : (device !== null && device.state === UPowerDeviceState.Charging)
  readonly property bool low: known && percent <= lowAt
  readonly property bool critical: known && percent <= criticalAt
  // the daemon knows the mouse's own name even when the charge comes from UPower
  readonly property string modelName: status && status.connected && status.model ? String(status.model) : (device && device.model ? device.model : "Mouse")

  readonly property string stateText: {
    if (!known) return ""
    if (charging) return "charging"
    if (!daemonHas && device && device.state === UPowerDeviceState.FullyCharged) return "full"
    return "discharging"
  }

  // ---- settings popup
  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
    if ("batteryText" in target) target.batteryText = root.batteryLine
  }
  // every mouse the daemon drives (status.mice); the top level of the status is the emptiest one
  readonly property var mice: status && status.connected === true && Array.isArray(status.mice) ? status.mice : []
  function miceLine(m) {
    return (m.percent !== undefined ? m.percent + "% · " + (m.charging === true ? "charging · " : "") : "") + String(m.model || "Mouse")
  }
  // the panel header, one line per mouse; any change in it (another mouse, a charge) goes to the panel
  readonly property string batteryLine: !root.connected ? "not connected"
    : root.mice.length > 1 ? root.mice.map(root.miceLine).join("\n")
    : (root.known ? root.percent + "% · " + (root.charging ? "charging · " : "") : "") + root.modelName
  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()
  onBatteryLineChanged: injectPanel()

  readonly property bool showPercent: panelLoader.item && panelLoader.item.val ? panelLoader.item.val("bar", "show_percent", true) === true : true
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  function open() { if (panelLoader.item && panelLoader.item.openFromHotkey) panelLoader.item.openFromHotkey() }
  function close() { if (panelLoader.item && panelLoader.item.close) panelLoader.item.close() }
  function togglePanel() { if (panelLoader.item && panelLoader.item.toggle) panelLoader.item.toggle() }
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false
  function closeForPopoutSwitch() { if (panelLoader.item) panelLoader.item.closeForPopoutSwitch() }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: { root.injectPanel(); Qt.callLater(root.injectPanel) }
  }

  IpcHandler {
    target: "io.github.2strange.magicmouse"
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  visible: connected
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.connected ? (root.charging ? "󰍽󱐋" : "󰍽") + (root.showPercent ? " " + (root.known ? root.percent + "%" : "–") : "") : ""
    active: root.low
    // amber while low, the theme's urgent red once critical (the theme has no amber of its own)
    activeColor: root.critical ? (root.bar ? root.bar.urgent : "#a55555") : Qt.tint(button.foreground, "#b0e0a030")
    tooltipText: !root.connected ? ""
      : root.mice.length > 1 ? root.mice.map(root.miceLine).join("\n")
      : root.modelName + (root.known ? " battery " + root.percent + "% (" + root.stateText + ")" : " battery: waiting for the mouse to report")
    onPressed: function(b) {
      if (!root.bar) return
      if (b === Qt.RightButton) root.bar.run("omarchy-shell shell toggle omarchy.bluetooth")
      else root.togglePanel()
    }
  }
}
