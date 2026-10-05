// Shared test plumbing: a private copy of the plugin per test (the way CodeConnect deploys it, next to its
// agent.json), a fake daemon on a unix socket, and small waits.
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { createServer } from "node:net"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { fileURLToPath, pathToFileURL } from "node:url"

export const PLUGIN = new URL("../codeconnect-opencode.js", import.meta.url)
export const FIXTURES = new URL("../../../../fixtures/opencode/", import.meta.url)
let sockets = 0
export const START = { sec: 1700000000, usec: 123456 }
// Inside node_modules, so the copy resolves `solid-js` the way the host resolves it for a plugin.
const SCRATCH = fileURLToPath(new URL("../node_modules/.cache/plugin-tests/", import.meta.url))

/** @param {{ agent?: boolean }} [opts] */
export async function loadPlugin(opts = {}) {
  mkdirSync(SCRATCH, { recursive: true })
  const dir = mkdtempSync(join(SCRATCH, "cc-oc-"))
  copyFileSync(PLUGIN, join(dir, "codeconnect-opencode.js"))
  if (opts.agent !== false) writeAgent(dir)
  const mod = await import(pathToFileURL(join(dir, "codeconnect-opencode.js")).href)
  const socket = join(tmpdir(), `cc-oc-${process.pid}-${++sockets}.sock`)
  const cleanup = () => {
    rmSync(dir, { recursive: true, force: true })
    rmSync(socket, { force: true })
  }
  return { mod, dir, socket, cleanup }
}

/** @param {string} dir */
export const writeAgent = (dir, pid = process.pid, start = START) => writeFileSync(join(dir, "agent.json"), JSON.stringify({ pid, start }))

export const sleep = (/** @type {number} */ ms) => new Promise((r) => setTimeout(r, ms))

/** @param {() => any} f @param {number} [ms] */
export async function until(f, ms = 5000) {
  const t0 = Date.now()
  for (;;) {
    const v = f()
    if (v) return v
    if (Date.now() - t0 > ms) throw new Error("timed out waiting")
    await sleep(5)
  }
}

/**
 * A fake daemon. `onHello(hello, conn)` answers the hello (default: a welcome with no acked roots).
 * @param {string} path
 * @param {{ onHello?: (hello: any, conn: any) => void, read?: boolean }} [opts]
 */
export function fakeCcd(path, opts = {}) {
  /** @type {any[]} */
  const conns = []
  const server = createServer((s) => {
    const conn = {
      socket: s,
      lines: /** @type {string[]} */ ([]),
      frames: /** @type {any[]} */ ([]),
      /** both directions, in the order this end saw them */
      wire: /** @type {{ dir: string, line: string }[]} */ ([]),
      closed: false,
      hello: null,
      send(/** @type {any} */ frame) {
        const line = JSON.stringify(frame) + "\n"
        conn.wire.push({ dir: "ccd", line })
        s.write(line)
      },
    }
    conns.push(conn)
    let buf = ""
    s.on("data", (d) => {
      buf += d.toString("utf8")
      let i
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        conn.lines.push(line + "\n")
        conn.wire.push({ dir: "plugin", line: line + "\n" })
        const f = JSON.parse(line)
        conn.frames.push(f)
        if (f.type === "opencode_hello") {
          conn.hello = f
          if (opts.onHello) opts.onHello(f, conn)
          else welcome(conn)
        }
      }
    })
    if (opts.read === false) s.pause()
    s.on("error", () => {})
    s.on("close", () => (conn.closed = true))
  })
  server.listen(path)
  return {
    conns,
    last: () => conns[conns.length - 1],
    close: () => new Promise((r) => {
      for (const c of conns) c.socket.destroy()
      server.close(() => r(undefined))
    }),
  }
}

/** @param {any} conn @param {any} [acked] */
export const welcome = (conn, acked = {}) => conn.send({ type: "opencode_welcome", link: "link-1", acked })
/** @param {any} conn @param {boolean} final */
export const refuse = (conn, final) => conn.send({ type: "opencode_refused", reason: "no", final })

/** @param {any} conn @param {number} sync */
export const settled = (conn, sync) => conn.frames.some((/** @type {any} */ f) => f.t === "settled" && f.sync === sync)

/** @param {string} name */
export const fixture = (name) => new URL(name, FIXTURES)
