import QtQuick
import qs.Commons
import qs.Ui
import "../Model.js" as Model

// The GPU the bar follows in full, then every other GPU as a row that
// takes over when clicked.
Column {
  id: root

  property var service: null
  property var host: null
  property var settings: ({})
  property string temperatureUnit: "Celsius"
  property bool publicIpEnabled: true
  property color foreground: Color.popups.text
  property string fontFamily: Style.font.family

  function flag(key) { return Model.flag(settings, key) }

  readonly property var snap: service ? service.snapshot : ({})
  readonly property var hist: service ? service.history : Model.emptyHistory()
  readonly property color s1: service ? service.series1 : Color.accent
  readonly property color s2: service ? service.series2 : Color.accent
  readonly property color warn: service ? service.warn : Color.accent
  readonly property color danger: service ? service.danger : Color.accent

  readonly property string source: String(Model.settingValue(settings, "gpuSource") || "auto")
  readonly property var gpus: Model.gpuList(snap)
  readonly property var gpu: Model.selectGpu(snap, source)
  readonly property var others: gpus.filter(function(one) { return Model.gpuKey(one) !== Model.gpuKey(root.gpu) })
  readonly property var gpuHist: Model.gpuHistory(hist, gpu)

  readonly property bool hasUtil: has("util")
  readonly property real memPercent: gpu && gpu.memTotal > 0 ? gpu.memUsed / gpu.memTotal * 100 : 0

  function has(key) {
    return !!gpu && gpu[key] !== null && gpu[key] !== undefined && isFinite(Number(gpu[key]))
  }

  function headerDetail(one) {
    if (!one) return ""
    var parts = []
    if (Model.gpuKindLabel(one)) parts.push(Model.gpuKindLabel(one))
    if (Model.freqText(one.mhz)) parts.push(Model.freqText(one.mhz))
    if (one.temp !== null && isFinite(Number(one.temp))) parts.push(Model.tempText(one.temp, temperatureUnit))
    return parts.join(", ")
  }

  function tempColor(celsius, max) {
    var frac = Model.num(celsius) / (max > 0 ? max : 95)
    if (frac >= 0.92) return danger
    if (frac >= 0.78) return warn
    return s1
  }

  function select(one) {
    if (host && typeof host.persist === "function") host.persist("gpuSource", Model.gpuKey(one))
  }

  width: parent ? parent.width : implicitWidth
  spacing: Style.space(10)

  Card {
    visible: !root.gpu
    foreground: root.foreground

    Text {
      textFormat: Text.PlainText
      width: parent.width
      text: "No GPU detected"
      color: root.foreground
      opacity: 0.6
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
    }
  }

  Card {
    visible: !!root.gpu
    foreground: root.foreground

    CardHeader {
      title: root.gpu ? Model.shortGpuName(root.gpu.name) : "GPU"
      detail: root.headerDetail(root.gpu)
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    HistoryGraph {
      width: parent.width
      height: Style.space(64)
      series: [root.gpuHist.util || []]
      colors: [root.s1]
      ceiling: 100
      baselineColor: Util.alpha(root.foreground, 0.14)
    }

    Item {
      width: parent.width
      height: rings.implicitHeight

      Row {
        id: rings
        anchors.horizontalCenter: parent.horizontalCenter
        spacing: Style.space(18)

        RingGauge {
          value: root.hasUtil ? Model.num(root.gpu.util) / 100 : 0
          color: root.s1
          foreground: root.foreground
          fontFamily: root.fontFamily
          topText: "Usage"
          valueText: root.hasUtil ? String(Math.round(root.gpu.util)) : "—"
          unitText: root.hasUtil ? "%" : ""
          subText: root.has("power") ? Math.round(root.gpu.power) + " W" : ""
          valueSize: Style.font.heading
          size: Style.space(84)
        }

        RingGauge {
          visible: !!(root.gpu && root.gpu.memTotal > 0)
          value: root.memPercent / 100
          color: root.s2
          foreground: root.foreground
          fontFamily: root.fontFamily
          topText: "Memory"
          valueText: String(Math.round(root.memPercent))
          unitText: "%"
          subText: root.gpu ? Model.bytesText(root.gpu.memUsed) : ""
          valueSize: Style.font.heading
          size: Style.space(84)
        }

        RingGauge {
          visible: root.has("temp")
          value: root.has("temp") ? Math.max(0, Math.min(1, root.gpu.temp / 100)) : 0
          color: root.has("temp") ? root.tempColor(root.gpu.temp, 90) : root.s1
          foreground: root.foreground
          fontFamily: root.fontFamily
          topText: "Temp"
          valueText: root.has("temp") ? Model.tempParts(root.gpu.temp, root.temperatureUnit).value : ""
          unitText: "°"
          subText: Model.gpuFanText(root.gpu).split(" · ")[0]
          valueSize: Style.font.heading
          size: Style.space(84)
        }
      }
    }

    Text {
      textFormat: Text.PlainText
      visible: !root.hasUtil && !!root.gpu
      width: parent.width
      text: "This driver does not report utilisation."
      color: root.foreground
      opacity: 0.45
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
      wrapMode: Text.WordWrap
    }
  }

  Card {
    visible: !!(root.gpu && (root.gpu.memTotal > 0 || root.gpu.gttTotal > 0)) && root.flag("showGpuMemory")
    foreground: root.foreground

    HistoryGraph {
      visible: !!(root.gpu && root.gpu.memTotal > 0)
      width: parent.width
      height: Style.space(40)
      series: [root.gpuHist.mem || []]
      colors: [root.s2]
      ceiling: 100
      baselineColor: Util.alpha(root.foreground, 0.14)
    }

    StatRow {
      visible: !!(root.gpu && root.gpu.memTotal > 0)
      label: "Video memory"
      dot: root.s2
      detail: Model.percentText(root.memPercent)
      value: root.gpu ? Model.pairText(root.gpu.memUsed, root.gpu.memTotal).replace(/ [A-Z]+$/, "") : ""
      unit: root.gpu ? Model.bytesParts(root.gpu.memTotal).unit : ""
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: !!(root.gpu && root.gpu.gttTotal > 0)
      label: "Shared memory"
      value: root.gpu ? Model.pairText(root.gpu.gttUsed, root.gpu.gttTotal).replace(/ [A-Z]+$/, "") : ""
      unit: root.gpu ? Model.bytesParts(root.gpu.gttTotal).unit : ""
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: root.has("memBusy")
      label: "Memory controller"
      value: root.has("memBusy") ? String(Math.round(root.gpu.memBusy)) : ""
      unit: "%"
      foreground: root.foreground
      fontFamily: root.fontFamily
    }
  }

  Card {
    visible: root.flag("showGpuSensors")
      && (root.has("power") || root.has("mhz") || root.has("memMhz") || root.has("tempJunction") || root.has("tempMem") || Model.gpuFanText(root.gpu) !== "")
    foreground: root.foreground

    StatRow {
      visible: root.has("power")
      label: "Power"
      value: root.has("power") ? String(Math.round(root.gpu.power)) : ""
      unit: "W"
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: root.has("mhz")
      label: "Core clock"
      detail: root.has("maxMhz") ? "of " + Model.freqText(root.gpu.maxMhz) : ""
      value: root.has("mhz") ? (Model.freqText(root.gpu.mhz) || "0 MHz").split(" ")[0] : ""
      unit: root.has("mhz") ? (Model.freqText(root.gpu.mhz) || "0 MHz").split(" ")[1] : ""
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: root.has("memMhz")
      label: "Memory clock"
      value: root.has("memMhz") ? (Model.freqText(root.gpu.memMhz) || "0 MHz").split(" ")[0] : ""
      unit: root.has("memMhz") ? (Model.freqText(root.gpu.memMhz) || "0 MHz").split(" ")[1] : ""
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: root.has("tempJunction")
      label: "Junction"
      value: root.has("tempJunction") ? Model.tempParts(root.gpu.tempJunction, root.temperatureUnit).value : ""
      unit: "°"
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: root.has("tempMem")
      label: "Memory temperature"
      value: root.has("tempMem") ? Model.tempParts(root.gpu.tempMem, root.temperatureUnit).value : ""
      unit: "°"
      foreground: root.foreground
      fontFamily: root.fontFamily
    }

    StatRow {
      visible: Model.gpuFanText(root.gpu) !== ""
      label: "Fan"
      value: Model.gpuFanText(root.gpu)
      foreground: root.foreground
      fontFamily: root.fontFamily
    }
  }

  Card {
    visible: root.others.length > 0 && root.flag("showGpuOthers")
    foreground: root.foreground

    SectionTitle { text: root.others.length > 1 ? "Other GPUs" : "Other GPU"; fontFamily: root.fontFamily }

    Repeater {
      model: root.others.length

      delegate: Item {
        id: row
        required property int index
        readonly property var modelData: root.others[index] || ({})
        readonly property bool hasUtil: modelData.util !== null && modelData.util !== undefined && isFinite(Number(modelData.util))

        width: parent ? parent.width : 0
        height: otherRow.height

        StatRow {
          id: otherRow
          width: parent.width
          label: Model.shortGpuName(row.modelData.name)
          dot: root.s1
          detail: root.headerDetail(row.modelData)
          value: row.hasUtil ? String(Math.round(row.modelData.util)) : "—"
          unit: row.hasUtil ? "%" : ""
          foreground: root.foreground
          fontFamily: root.fontFamily
        }

        MouseArea {
          id: hover
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          onClicked: root.select(row.modelData)
        }

        Rectangle {
          anchors.fill: parent
          anchors.leftMargin: -Style.space(6)
          anchors.rightMargin: -Style.space(6)
          z: -1
          radius: Style.cornerRadius
          color: hover.containsMouse ? Style.hoverFillFor(root.foreground, Color.accent) : "transparent"
        }

        PanelToolTip {
          visible: hover.containsMouse
          text: [row.modelData.name, row.modelData.driver, row.modelData.id].filter(function(v) { return !!v }).join("\n") + "\nClick to follow this GPU"
          fontFamily: root.fontFamily
        }
      }
    }
  }
}
