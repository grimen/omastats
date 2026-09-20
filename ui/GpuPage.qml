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
  // Shown inside the CPU page while the GPU tab is off: no process list there,
  // because the sampler only gathers GPU processes for the GPU tab.
  property bool embedded: false

  function flag(key) { return Model.flag(settings, key) }

  readonly property var snap: service ? service.snapshot : ({})
  readonly property var hist: service ? service.history : Model.emptyHistory()
  readonly property color s1: service ? service.series1 : Color.accent
  readonly property color s2: service ? service.series2 : Color.accent
  readonly property color warn: service ? service.warn : Color.urgent
  readonly property color danger: service ? service.danger : Color.urgent
  readonly property var gpuProcs: snap.gpuProcs

  // Drivers leave out what they cannot measure, so every figure is optional.
  function has(gpu, key) {
    return !!gpu && gpu[key] !== null && gpu[key] !== undefined && isFinite(Number(gpu[key]))
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
      readonly property real memPercent: gpu && gpu.memTotal > 0 ? gpu.memUsed / gpu.memTotal * 100 : 0
      // The hotspot is what the card throttles on; the edge sensor reads cooler.
      readonly property string tempKey: root.has(gpu, "tempJunction") ? "tempJunction" : "temp"
      readonly property bool powerLimited: root.has(gpu, "power") && root.has(gpu, "powerCap") && gpu.powerCap > 0
      foreground: root.foreground

      CardHeader {
        title: Model.gpuKindLabel(gpuCard.gpu)
        detail: !gpuCard.gpu ? "" : gpuCard.gpu.asleep
          ? Model.shortGpuName(gpuCard.gpu.name) + ", asleep"
          : [Model.shortGpuName(gpuCard.gpu.name), Model.pcieText(gpuCard.gpu)].filter(function(part) { return part }).join(", ")
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      Item {
        visible: !!gpuCard.gpu && !gpuCard.gpu.asleep
        width: parent.width
        height: rings.implicitHeight

        Row {
          id: rings
          anchors.horizontalCenter: parent.horizontalCenter
          spacing: Style.space(18)

          RingGauge {
            visible: root.has(gpuCard.gpu, "util")
            value: visible ? gpuCard.gpu.util / 100 : 0
            color: root.s1
            foreground: root.foreground
            fontFamily: root.fontFamily
            topText: "Usage"
            valueText: visible ? String(Math.round(gpuCard.gpu.util)) : ""
            unitText: "%"
            subText: gpuCard.gpu ? Model.freqText(gpuCard.gpu.mhz) : ""
            valueSize: Style.font.heading
            size: Style.space(84)
          }

          RingGauge {
            visible: !!(gpuCard.gpu && gpuCard.gpu.memTotal > 0)
            value: gpuCard.memPercent / 100
            color: root.s2
            foreground: root.foreground
            fontFamily: root.fontFamily
            topText: "Memory"
            valueText: String(Math.round(gpuCard.memPercent))
            unitText: "%"
            subText: visible ? Model.bytesText(gpuCard.gpu.memUsed) : ""
            valueSize: Style.font.heading
            size: Style.space(84)
          }

          RingGauge {
            // Only with a known limit is power a fraction; otherwise it is a row below.
            visible: gpuCard.powerLimited
            value: visible ? Math.max(0, Math.min(1, gpuCard.gpu.power / gpuCard.gpu.powerCap)) : 0
            color: visible && value >= 0.92 ? root.danger : visible && value >= 0.78 ? root.warn : root.s1
            foreground: root.foreground
            fontFamily: root.fontFamily
            topText: "Power"
            valueText: visible ? String(Math.round(gpuCard.gpu.power)) : ""
            unitText: "W"
            subText: visible ? "of " + Math.round(gpuCard.gpu.powerCap) + " W" : ""
            valueSize: Style.font.heading
            size: Style.space(84)
          }
        }
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
        visible: !!(gpuCard.gpu && gpuCard.gpu.memTotal > 0)
        label: "Memory"
        detail: visible ? Model.percentText(gpuCard.memPercent) : ""
        value: visible ? Model.pairText(gpuCard.gpu.memUsed, gpuCard.gpu.memTotal).replace(/ [A-Z]+$/, "") : ""
        unit: visible ? Model.bytesParts(gpuCard.gpu.memTotal).unit : ""
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, "memBusy")
        label: "Memory controller"
        value: visible ? String(Math.round(gpuCard.gpu.memBusy)) : ""
        unit: "%"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, "vcnBusy")
        label: "Video engine"
        value: visible ? String(Math.round(gpuCard.gpu.vcnBusy)) : ""
        unit: "%"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, "power") && !gpuCard.powerLimited
        label: "Power"
        value: visible ? String(Math.round(gpuCard.gpu.power)) : ""
        unit: "W"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, gpuCard.tempKey)
        label: "Temperature"
        detail: gpuCard.tempKey === "tempJunction" ? "hotspot" : ""
        value: visible ? Model.tempParts(gpuCard.gpu[gpuCard.tempKey], root.temperatureUnit).value : ""
        unit: "°"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, "memMhz")
        label: "Memory clock"
        value: visible ? String(Math.round(gpuCard.gpu.memMhz)) : ""
        unit: "MHz"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        visible: root.has(gpuCard.gpu, "tempMem")
        label: "Memory temperature"
        value: visible ? Model.tempParts(gpuCard.gpu.tempMem, root.temperatureUnit).value : ""
        unit: "°"
        foreground: root.foreground
        fontFamily: root.fontFamily
      }

      StatRow {
        // sysfs gives a speed, nvidia-smi only a duty cycle.
        readonly property bool rpm: root.has(gpuCard.gpu, "fanRpm")
        visible: rpm || root.has(gpuCard.gpu, "fan")
        label: "Fan"
        detail: rpm && gpuCard.gpu.fanRpm > 0 && gpuCard.gpu.fanMax > 0 ? Model.percentText(gpuCard.gpu.fanRpm / gpuCard.gpu.fanMax * 100) : ""
        value: !visible ? "" : !rpm ? String(Math.round(gpuCard.gpu.fan)) : gpuCard.gpu.fanRpm > 0 ? String(Math.round(gpuCard.gpu.fanRpm)) : "Stopped"
        unit: !visible ? "" : !rpm ? "%" : gpuCard.gpu.fanRpm > 0 ? "rpm" : ""
        foreground: root.foreground
        fontFamily: root.fontFamily
      }
    }
  }

  Card {
    visible: !root.embedded && root.flag("showProcesses")
    foreground: root.foreground

    ProcessList {
      host: root.host
      expandable: false
      caption: "GPU time and video memory per process, from the kernel's DRM client statistics. NVIDIA's driver does not publish them."
      items: Array.isArray(root.gpuProcs) ? root.gpuProcs : []
      total: items.length
      sortKey: "gpu"
      columns: [
        { key: "gpu", kind: "percent", title: "GPU" },
        { key: "vram", kind: "bytes", title: "Memory" }
      ]
      columnWidth: Style.space(70)
      emptyText: Array.isArray(root.gpuProcs) ? "No GPU clients" : "Measuring…"
      foreground: root.foreground
      fontFamily: root.fontFamily
    }
  }
}
