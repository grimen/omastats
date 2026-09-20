import QtQuick
import qs.Commons
import "../Model.js" as Model

Column {
  id: root

  property var service: null
  property var host: null
  property var settings: ({})
  property string temperatureUnit: "Celsius"
  property bool publicIpEnabled: true
  property color foreground: Color.popups.text
  property string fontFamily: Style.font.family

  readonly property var snap: service ? service.snapshot : ({})
  readonly property var hist: service ? service.history : Model.emptyHistory()
  readonly property color s1: service ? service.series1 : Color.accent

  function headerDetail(mhz, temp) {
    var parts = []
    var freq = Model.freqText(mhz)
    if (freq) parts.push(freq)
    if (isFinite(Number(temp)) && temp !== null) parts.push(Model.tempText(temp, temperatureUnit))
    return parts.join(", ")
  }

  width: parent ? parent.width : implicitWidth
  spacing: Style.space(10)

  Repeater {
    // One card per GPU; a count model keeps the cards alive between samples.
    model: Model.gpuList(root.snap).length

    Card {
      id: gpuCard
      required property int index
      readonly property var gpu: Model.gpuList(root.snap)[index] || null
      foreground: root.foreground

      CardHeader {
        title: Model.gpuKindLabel(gpuCard.gpu)
        detail: !gpuCard.gpu ? "" : gpuCard.gpu.asleep ? "Asleep" : root.headerDetail(gpuCard.gpu.mhz, gpuCard.gpu.temp)
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      HistoryGraph {
        width: parent.width
        height: Style.space(48)
        series: [(root.hist.gpus ? root.hist.gpus[Model.gpuKey(gpuCard.gpu)] : null) || []]
        colors: [root.s1]
        ceiling: 100
        baselineColor: Util.alpha(root.foreground, 0.14)
      }

      StatRow {
        label: gpuCard.gpu ? Model.shortGpuName(gpuCard.gpu.name) : "Processor"
        dot: root.s1
        value: gpuCard.gpu && isFinite(Number(gpuCard.gpu.util)) ? String(Math.round(gpuCard.gpu.util)) : "—"
        unit: gpuCard.gpu && isFinite(Number(gpuCard.gpu.util)) ? "%" : ""
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: !!(gpuCard.gpu && gpuCard.gpu.memTotal > 0)
        label: "Memory"
        detail: gpuCard.gpu && gpuCard.gpu.memTotal > 0 ? Model.percentText(gpuCard.gpu.memUsed / gpuCard.gpu.memTotal * 100) : ""
        value: gpuCard.gpu ? Model.pairText(gpuCard.gpu.memUsed, gpuCard.gpu.memTotal).replace(/ [A-Z]+$/, "") : ""
        unit: gpuCard.gpu ? Model.bytesParts(gpuCard.gpu.memTotal).unit : ""
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: !!(gpuCard.gpu && isFinite(Number(gpuCard.gpu.power)) && gpuCard.gpu.power !== null)
        label: "Power"
        value: gpuCard.gpu && gpuCard.gpu.power !== null ? String(Math.round(gpuCard.gpu.power)) : ""
        unit: "W"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }
    }
  }
}
