// A stand-in for the CodeConnect daemon's side of the OpenCode link, for the live tests. It listens on a unix
// socket, admits a hello only from the process named in the session's agent.json (same pid, same start time as
// `ps` reports it, the session's nonce, the expected folder), answers with `acked` derived from what it has recorded,
// and appends every frame it reads to a JSONL record. A restart reads the record back, as the daemon reads its
// store. SIGUSR1 cuts the current link; SIGUSR2 asks for a resync. Being its own process, it can be SIGSTOPped.
//
// Usage: bun fake-ccd.js <socket> <session dir> <directory> <record.jsonl> [<acked JSON while nothing is recorded>]
import { appendFileSync, existsSync, readFileSync, rmSync } from "node:fs"
import { createServer } from "node:net"
import { join } from "node:path"
import { birth } from "./proc.js"

/**
 * The acked bound per root, from the recorded frames: the newest turn that ended, a turn being the user message
 * that started it. A turn has ended when its root went idle after it, or when an assistant reply to it finished
 * with "stop".
 * @param {any[]} frames
 */
export function ackedFrom(frames) {
  /** @type {Map<string, string>} */ const parent = new Map()
  /** @type {Map<string, string>} */ const lastUser = new Map()
  /** @type {Record<string, { from: string, inclusive: boolean }>} */ const acked = {}
  const close = (/** @type {string} */ s, /** @type {string} */ turn) => {
    if (!acked[s] || turn > acked[s].from) acked[s] = { from: turn, inclusive: false }
  }
  const info = (/** @type {any} */ i) => {
    if (!i || parent.has(i.sessionID)) return
    if (i.role === "user" && (!lastUser.get(i.sessionID) || i.id > /** @type {string} */ (lastUser.get(i.sessionID)))) lastUser.set(i.sessionID, i.id)
    if (i.role === "assistant" && i.time?.completed && i.finish === "stop" && i.parentID) close(i.sessionID, i.parentID)
  }
  for (const f of frames) {
    if (f.t === "ev" && f.type === "session.created" && f.properties.info?.parentID) parent.set(f.properties.info.id, f.properties.info.parentID)
    if (f.t === "ev" && f.type === "message.updated") info(f.properties.info)
    if (f.t === "sync_page")
      for (const it of f.items) {
        if (it.session?.parentID) parent.set(it.session.id, it.session.parentID)
        if (it.info) info(it.info)
      }
    if (f.t === "ev" && f.type === "session.idle") {
      const s = f.properties.sessionID
      if (lastUser.has(s)) close(s, /** @type {string} */ (lastUser.get(s)))
    }
  }
  return acked
}

if (import.meta.main) main()

function main() {
  const [socketPath, sessionDir, directory, recordPath, initial] = process.argv.slice(2)
  const recorded = () => (existsSync(recordPath) ? readFileSync(recordPath, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [])
  const frames = recorded().map((r) => r.frame).filter(Boolean)
  const process0 = recorded().reduce((n, r) => Math.max(n, r.process ?? 0), -1) + 1
  const record = (/** @type {any} */ o) => appendFileSync(recordPath, JSON.stringify({ process: process0, t: Date.now(), ...o }) + "\n")

  /** @type {import("node:net").Socket | null} */
  let live = null
  let conns = 0
  rmSync(socketPath, { force: true })
  const server = createServer((s) => {
    const conn = ++conns
    let buf = ""
    let admitted = false
    const send = (/** @type {any} */ f) => {
      s.write(JSON.stringify(f) + "\n")
      record({ conn, sent: f })
    }
    s.on("data", (d) => {
      buf += d.toString("utf8")
      let i
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        const bytes = Buffer.byteLength(line) + 1
        let f
        try {
          f = JSON.parse(line)
        } catch {
          record({ conn, bad: bytes })
          continue
        }
        if (!admitted) {
          const why = refuse(f)
          record({ conn, bytes, frame: f, admitted: !why, why })
          if (why) {
            send({ type: "opencode_refused", reason: why, final: true })
            s.end()
            return
          }
          admitted = true
          if (live) live.destroy()
          live = s
          send({ type: "opencode_welcome", link: `link-${process0}-${conn}`, acked: frames.length || !initial ? ackedFrom(frames) : JSON.parse(initial) })
          continue
        }
        frames.push(f)
        record({ conn, bytes, frame: f })
      }
    })
    s.on("error", () => {})
    s.on("close", () => {
      if (live === s) live = null
      record({ conn, closed: true })
    })
  })

  /** @param {any} h */
  function refuse(h) {
    if (h.type !== "opencode_hello" || h.wire !== 1) return "not a wire-1 hello"
    const cfg = JSON.parse(readFileSync(join(sessionDir, "tui.json"), "utf8"))
    if (h.nonce !== cfg.plugin[0][1].nonce) return "unknown nonce"
    const agent = JSON.parse(readFileSync(join(sessionDir, "agent.json"), "utf8"))
    if (h.pid !== agent.pid) return "wrong pid"
    const b = birth(h.pid)
    if (JSON.stringify(h.start) !== JSON.stringify(agent.start) || JSON.stringify(b) !== JSON.stringify(agent.start)) return "stale start time"
    if (h.directory !== directory) return "another folder"
    return null
  }

  process.on("SIGUSR1", () => {
    record({ op: "cut" })
    live?.destroy()
  })
  process.on("SIGUSR2", () => {
    if (!live) return
    const f = { type: "opencode_resync", acked: ackedFrom(frames) }
    live.write(JSON.stringify(f) + "\n")
    record({ op: "resync", sent: f })
  })
  server.listen(socketPath, () => console.log("listening"))
}
