import QtQuick
import Quickshell
import Quickshell.Io

// Answers xdg-desktop-portal-omarchy over
// $XDG_RUNTIME_DIR/xdg-desktop-portal-omarchy.sock: one NDJSON request line
// in, one response line out, then the plugin closes the connection. A
// client that disconnects before the answer (the portal request was
// closed) cancels its dialog. Only one request is served at a time; others
// get "busy", which the backend treats as a refusal.
//
// Protocol v1:
//   {"v":1,"type":"clipboard","app_id":"…","app_name":"…","app_names"?:{"pt_BR":"…"}}
//   -> {"result":"allow"|"always"|"deny"|"busy"|"error"}
Item {
  id: root

  readonly property int protocolVersion: 1
  readonly property int maxRequestBytes: 4096
  property var activeConn: null

  function reply(conn, response) {
    conn.write(JSON.stringify(response) + "\n")
    conn.flush()
    conn.connected = false
  }

  function handleRequest(conn, line) {
    if (conn.handled) return
    conn.handled = true

    if (line.length > maxRequestBytes) {
      reply(conn, { result: "error", message: "request too large" })
      return
    }
    var req
    try {
      req = JSON.parse(line)
    } catch (e) {
      reply(conn, { result: "error", message: "invalid JSON" })
      return
    }
    if (!req || req.v !== protocolVersion || req.type !== "clipboard" || typeof req.app_id !== "string" || req.app_id.length === 0) {
      reply(conn, { result: "error", message: "unsupported request" })
      return
    }
    if (activeConn) {
      reply(conn, { result: "busy" })
      return
    }
    activeConn = conn
    dialog.open(req)
  }

  function finish(result) {
    var conn = activeConn
    activeConn = null
    if (!conn) return
    reply(conn, { result: result })
  }

  PortalDialog {
    id: dialog
    onFinished: function(result) { root.finish(result) }
  }

  SocketServer {
    active: true
    path: Quickshell.env("XDG_DESKTOP_PORTAL_OMARCHY_SOCKET") || (Quickshell.env("XDG_RUNTIME_DIR") + "/xdg-desktop-portal-omarchy.sock")
    handler: Socket {
      id: conn
      property bool handled: false

      onConnectionStateChanged: {
        if (!connected && root.activeConn === conn) {
          root.activeConn = null
          dialog.close()
        }
      }

      parser: SplitParser {
        onRead: function(line) { root.handleRequest(conn, line) }
      }
    }
  }
}
