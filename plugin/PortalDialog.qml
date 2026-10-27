import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// Overlay dialog modelled on the shell's polkit agent
// (/usr/share/omarchy/shell/plugins/polkit/PolkitAgent.qml) and omapin's:
// same layer, scrim, card surface and [polkit] colours. The app's name
// comes from its desktop entry, so the app id is always shown too, as
// plain text. Texts align left explicitly, so that right-to-left languages
// mirror them all (the app id included). Colours go through Commons.Color, as in PolkitAgent.qml:
// a bare `Color` doesn't resolve to the Commons singleton.
Item {
  id: root

  signal finished(string result)

  // The dialog's strings (plugin/i18n/), in the language picked under
  // Setup > Language.
  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

  property string appId: ""
  property string defaultAppName: ""
  // The desktop entry's Name[locale] values, by locale (pt_BR, sr@latin).
  property var appNames: ({})
  readonly property string appName: localizedName(Commons.I18n.language) || defaultAppName || appId

  property bool shown: false
  property bool closing: false
  // Allowing takes a moment after the dialog appears: it grabs the
  // keyboard at once, so an Enter (or a click) meant for another window
  // would otherwise grant access unread. Denying works right away.
  property bool armed: false
  readonly property int armDelay: 500

  property string fontFamily: Style.font.menuFamily
  property color accent: Commons.Color.polkit.accent
  property color background: Commons.Color.polkit.background
  property color foreground: Commons.Color.polkit.text
  property var borderSpec: Border.surfaceSpec("polkit", "border", Commons.Color.polkit.border, Math.max(1, Style.space(2)), "border-alpha")
  readonly property int contentMargin: Style.spacing.panelPadding
  readonly property int cardWidth: Math.min(Style.space(460), Math.max(Style.space(280), panel.width - Style.gapsOut * 2))

  function open(req) {
    closeTimer.stop()
    closing = false
    appId = String(req.app_id || "")
    defaultAppName = String(req.app_name || "")
    armed = false
    armTimer.restart()
    appNames = (req.app_names && typeof req.app_names === "object" && !Array.isArray(req.app_names)) ? req.app_names : ({})
    shown = true
    Qt.callLater(refocus)
  }

  // Hides the dialog without answering (the client went away).
  function close() {
    if (!shown) return
    closing = true
    shown = false
    closeTimer.restart()
  }

  function respond(result) {
    if (!shown) return
    if (result !== "deny" && !armed) return
    close()
    finished(result)
  }

  // The shell's language code (pt-BR) as a desktop entry locale (pt_BR),
  // then its base language (pt).
  function localizedName(language) {
    var code = String(language || "").replace("-", "_")
    var candidates = [code, code.split("_")[0]]
    for (var i = 0; i < candidates.length; i++) {
      var name = root.appNames[candidates[i]]
      if (typeof name === "string" && name.length > 0) return name
    }
    return ""
  }

  function refocus() {
    if (shown) keyCatcher.forceActiveFocus()
  }

  Timer {
    id: armTimer
    interval: root.armDelay
    repeat: false
    onTriggered: root.armed = true
  }

  Timer {
    id: closeTimer
    interval: 300
    repeat: false
    onTriggered: root.closing = false
  }

  PanelWindow {
    id: panel
    // Right-to-left languages read this window mirrored.
    Binding { target: panel.contentItem; property: "LayoutMirroring.enabled"; value: Commons.I18n.rightToLeft }
    Binding { target: panel.contentItem; property: "LayoutMirroring.childrenInherit"; value: true }

    visible: root.shown || root.closing
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    WlrLayershell.namespace: "xdg-desktop-portal-omarchy"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: root.shown ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None
    exclusionMode: ExclusionMode.Ignore

    Rectangle {
      anchors.fill: parent
      color: Commons.Color.polkit.scrim
    }

    MouseArea {
      anchors.fill: parent
      onClicked: root.refocus()
    }

    BorderSurface {
      id: card
      width: root.cardWidth
      height: content.implicitHeight + card.contentTopInset + card.contentBottomInset
      radius: Style.cornerRadius
      anchors.centerIn: parent
      color: root.background
      borderSpec: root.borderSpec
      padding: root.contentMargin

      MouseArea { anchors.fill: parent; onClicked: root.refocus() }

      Item {
        id: keyCatcher
        anchors.fill: parent
        focus: true

        // Escape refuses; Enter allows this once. "Always" takes a click.
        Keys.priority: Keys.BeforeItem
        Keys.onPressed: function(event) {
          if (event.key === Qt.Key_Escape) {
            root.respond("deny")
            event.accepted = true
          } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
            // A held Enter repeats past the delay; only a fresh press counts.
            if (!event.isAutoRepeat) root.respond("allow")
            event.accepted = true
          }
        }
      }

      Column {
        id: content
        anchors.fill: parent
        anchors.topMargin: card.contentTopInset
        anchors.rightMargin: card.contentRightInset
        anchors.bottomMargin: card.contentBottomInset
        anchors.leftMargin: card.contentLeftInset
        spacing: Style.space(10)

        Row {
          width: parent.width
          spacing: Style.space(14)

          Text {
            id: icon
            text: "\uf0ea"
            color: root.accent
            font.family: root.fontFamily
            font.pixelSize: Style.font.iconLarge
            width: Style.space(26)
            horizontalAlignment: Text.AlignHCenter
          }

          Column {
            width: parent.width - icon.width - parent.spacing
            spacing: Style.space(4)

            Text {
              width: parent.width
              textFormat: Text.PlainText
              // The English fallback keeps a %1 for arg() before the catalogs load.
              text: strings.t("portal.wantsClipboard", "%1 wants to use the clipboard").arg(root.appName)
              wrapMode: Text.Wrap
              horizontalAlignment: Text.AlignLeft
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.body
            }

            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.appId
              elide: Text.ElideMiddle
              horizontalAlignment: Text.AlignLeft
              color: root.foreground
              opacity: 0.6
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }
          }
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          text: strings.t("portal.explanation")
          wrapMode: Text.Wrap
          horizontalAlignment: Text.AlignLeft
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Row {
          anchors.right: parent.right
          spacing: Style.spacing.controlGap

          Button {
            text: strings.t("portal.deny")
            bordered: true
            onClicked: root.respond("deny")
          }
          Button {
            text: strings.t("portal.alwaysAllow")
            bordered: true
            onClicked: root.respond("always")
          }
          Button {
            text: strings.t("portal.allowOnce")
            bordered: true
            selected: true
            onClicked: root.respond("allow")
          }
        }
      }
    }

    Rectangle {
      width: Math.min(titleText.implicitWidth + Style.space(24), panel.width - Style.gapsOut * 2)
      height: Style.space(28)
      anchors.horizontalCenter: card.horizontalCenter
      anchors.bottom: card.top
      anchors.bottomMargin: Style.space(10)
      radius: Style.cornerRadius
      color: root.background

      Text {
        id: titleText
        textFormat: Text.PlainText
        anchors.fill: parent
        anchors.leftMargin: Style.space(12)
        anchors.rightMargin: Style.space(12)
        text: strings.t("portal.title")
        color: root.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        horizontalAlignment: Text.AlignHCenter
        verticalAlignment: Text.AlignVCenter
        elide: Text.ElideMiddle
      }
    }
  }
}
