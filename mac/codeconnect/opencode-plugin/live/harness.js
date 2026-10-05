// Live-test rig: a real OpenCode TUI in a private tmux server, the scripted mock model, and fake-ccd.js, all under
// one temporary directory that is removed afterwards.
//
// Gate: nothing runs unless CC_OPENCODE_LIVE=1. With the flag set, a missing OpenCode binary is a failure, not a
// skip. The binary is CC_OPENCODE_BIN, else `opencode` on PATH; the tests are written against OpenCode 1.18.34.
import { spawn, spawnSync } from "node:child_process"
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { delimiter, join } from "node:path"
import { fileURLToPath } from "node:url"
import { ackedFrom, birth } from "./fake-ccd.js"

export const LIVE = process.env.CC_OPENCODE_LIVE === "1"
if (!LIVE) console.error("SKIP live OpenCode tests: set CC_OPENCODE_LIVE=1 (and CC_OPENCODE_BIN) to run them")

const PLUGIN = fileURLToPath(new URL("../codeconnect-opencode.js", import.meta.url))
const FAKE_CCD = fileURLToPath(new URL("./fake-ccd.js", import.meta.url))
const MOCK = fileURLToPath(new URL("./mock-model.js", import.meta.url))
export const NONCE = "0123456789abcdef0123456789abcdef"
export const sleep = (/** @type {number} */ ms) => new Promise((r) => setTimeout(r, ms))

export function opencodeBin() {
  const env = process.env.CC_OPENCODE_BIN
  if (env) {
    if (!existsSync(env)) throw new Error(`CC_OPENCODE_LIVE=1 but CC_OPENCODE_BIN=${env} does not exist`)
    return env
  }
  for (const d of (process.env.PATH ?? "").split(delimiter)) if (d && existsSync(join(d, "opencode"))) return join(d, "opencode")
  throw new Error("CC_OPENCODE_LIVE=1 but no OpenCode binary was found: set CC_OPENCODE_BIN rather than pass vacuously")
}

/** The plugin source with `code` inserted after `anchor` (which must occur exactly once). */
export function variant(/** @type {[string, string][]} */ edits, source = readFileSync(PLUGIN, "utf8")) {
  for (const [anchor, code] of edits) {
    const i = source.indexOf(anchor)
    if (i < 0 || source.indexOf(anchor, i + 1) >= 0) throw new Error("anchor not found exactly once: " + anchor)
    source = source.slice(0, i + anchor.length) + code + source.slice(i + anchor.length)
  }
  return source
}

/** A build that records the queue and frame bounds it reached, and its listeners' main-thread time, into `<session dir>/stats.json`. */
export const instrumented = () =>
  variant([
    [
      'import { fileURLToPath } from "node:url"\n',
      `import { writeFileSync as __w } from "node:fs"
const __st = { maxQueue: 0, maxFrame: 0, maxPage: 0, drops: 0, frames: 0 }
const __q = (/** @type {any} */ c, /** @type {number} */ extra) => { __st.maxQueue = Math.max(__st.maxQueue, (c?.writableLength ?? 0) + extra) }
const __t = { listenerCalls: 0, listenerMs: 0, listenerMaxMs: 0, loopLagMaxMs: 0 }
Object.assign(__st, { time: __t })
let __last = performance.now()
setInterval(() => { const n = performance.now(); __t.loopLagMaxMs = Math.max(__t.loopLagMaxMs, n - __last - 20); __last = n }, 20).unref?.()
setInterval(() => { try { __w(fileURLToPath(new URL("./stats.json", import.meta.url)), JSON.stringify(__st)) } catch {} }, 200).unref?.()
`,
    ],
    ["  function enqueue(e, s) {\n    const c = sock\n", "    __st.frames++; __st.maxFrame = Math.max(__st.maxFrame, e.bytes); __q(c, heldBytes + e.bytes)\n"],
    ["  function overflow(c) {\n", "    __st.drops++\n"],
    [
      "  function forward(type, event) {\n",
      "    const __t0 = performance.now()\n    try {\n      return __forward(type, event)\n    } finally {\n      const d = performance.now() - __t0\n      __t.listenerCalls++\n      __t.listenerMs += d\n      __t.listenerMaxMs = Math.max(__t.listenerMaxMs, d)\n    }\n  }\n  function __forward(type, event) {\n",
    ],
    ["      pages++\n", "      __st.maxPage = Math.max(__st.maxPage, utf8(line)); __q(c, utf8(line) + heldBytes)\n"],
  ])

let rigs = 0
export class Rig {
  constructor() {
    this.bin = opencodeBin()
    this.root = mkdtempSync(join(tmpdir(), "cc-oc-live-"))
    this.home = join(this.root, "home")
    this.proj = join(this.root, "proj")
    this.sess = join(this.root, "sess")
    this.socket = join(this.root, "ccd.sock")
    this.record = join(this.root, "record.jsonl")
    this.tmux = ["-L", `cc-oc-live-${process.pid}-${++rigs}`, "-f", "/dev/null"]
    /** @type {import("node:child_process").ChildProcess | null} */
    this.ccd = null
    /** @type {import("node:child_process").ChildProcess | null} */
    this.mock = null
    this.port = 0
    this.pid = 0
    for (const d of [this.home, this.proj, this.sess]) mkdirSync(d, { recursive: true })
    writeFileSync(join(this.proj, "a.txt"), "hello\n")
    spawnSync("git", ["init", "-q"], { cwd: this.proj })
    spawnSync("git", ["-c", "user.email=t@example.invalid", "-c", "user.name=t", "commit", "-qm", "init", "--allow-empty"], { cwd: this.proj })
  }

  /** The mock model runs in its own process, so a busy test loop cannot slow its stream. */
  async start(permission = { "*": "allow", bash: "allow" }) {
    const p = spawn(process.execPath, [MOCK], { stdio: ["ignore", "pipe", "inherit"] })
    this.mock = p
    this.port = await new Promise((resolve, reject) => {
      p.stdout?.once("data", (d) => resolve(Number(/listening (\d+)/.exec(String(d))?.[1])))
      p.once("exit", () => reject(new Error("mock model exited")))
    })
    this.config(permission)
    return this
  }

  /** @param {Record<string, string>} permission */
  config(permission) {
    const dir = join(this.home, ".config", "opencode")
    mkdirSync(dir, { recursive: true })
    writeFileSync(
      join(dir, "opencode.json"),
      JSON.stringify({
        $schema: "https://opencode.ai/config.json",
        autoupdate: false,
        share: "disabled",
        model: "mock/mock-model",
        small_model: "mock/mock-model",
        provider: {
          mock: {
            npm: "@ai-sdk/openai-compatible",
            name: "Mock",
            options: { baseURL: `http://127.0.0.1:${this.port}/v1`, apiKey: "x" },
            models: { "mock-model": { name: "Mock Model", tool_call: true, limit: { context: 1000000, output: 100000 } } },
          },
        },
        permission,
      }),
    )
  }

  env() {
    /** @type {Record<string, string>} */
    const env = { HOME: this.home, PATH: process.env.PATH ?? "", TERM: "xterm-256color", OPENCODE_DISABLE_AUTOUPDATE: "1", OPENCODE_DISABLE_MODELS_FETCH: "1", NO_PROXY: "127.0.0.1,localhost", no_proxy: "127.0.0.1,localhost" }
    return env
  }

  /** @param {any} [acked] the first welcome's acked roots, used while nothing is recorded */
  async startCcd(acked) {
    const args = [FAKE_CCD, this.socket, this.sess, this.proj, this.record, ...(acked ? [JSON.stringify(acked)] : [])]
    const p = spawn(process.execPath, args, { stdio: ["ignore", "pipe", "inherit"] })
    this.ccd = p
    await new Promise((resolve, reject) => {
      p.stdout?.once("data", resolve)
      p.once("exit", () => reject(new Error("fake-ccd exited")))
    })
    return p
  }

  stopCcd() {
    this.ccd?.kill("SIGKILL")
    this.ccd = null
  }

  /** @param {string} sig */
  signalCcd(sig) {
    if (this.ccd?.pid) process.kill(this.ccd.pid, sig)
  }

  /**
   * Writes the session files and starts OpenCode in the pane. `plugin`: "real", "none", or a plugin source.
   * @param {{ plugin?: string, args?: string[], agent?: boolean }} [o]
   */
  async launch(o = {}) {
    const plugin = o.plugin ?? "real"
    rmSync(join(this.sess, "agent.json"), { force: true })
    rmSync(join(this.sess, "stats.json"), { force: true })
    if (plugin === "none") writeFileSync(join(this.sess, "tui.json"), JSON.stringify({ plugin: [] }))
    else {
      writeFileSync(join(this.sess, "codeconnect-opencode.js"), plugin === "real" ? readFileSync(PLUGIN) : plugin)
      writeFileSync(join(this.sess, "tui.json"), JSON.stringify({ plugin: [["./codeconnect-opencode.js", { socket: this.socket, nonce: NONCE }]] }))
    }
    const env = Object.entries({ ...this.env(), OPENCODE_TUI_CONFIG: join(this.sess, "tui.json") }).map(([k, v]) => `${k}=${v}`)
    this.t(["kill-server"])
    const r = this.t(["new-session", "-d", "-s", "oc", "-x", "160", "-y", "50", "-c", this.proj, "--", "env", "-i", ...env, this.bin, ...(o.args ?? [])])
    if (r.status !== 0) throw new Error("tmux: " + r.stderr)
    this.pid = Number(this.t(["display", "-p", "-t", "oc", "#{pane_pid}"]).stdout.trim())
    if (o.agent !== false) this.writeAgent()
    return this.pid
  }

  writeAgent() {
    const tmp = join(this.sess, "agent.json.tmp")
    writeFileSync(tmp, JSON.stringify({ pid: this.pid, start: birth(this.pid) }), { flag: "wx", mode: 0o600 })
    renameSync(tmp, join(this.sess, "agent.json"))
  }

  /** @param {string[]} args */
  t(args) {
    return spawnSync("tmux", [...this.tmux, ...args], { encoding: "utf8" })
  }
  capture() {
    return this.t(["capture-pane", "-p", "-t", "oc"]).stdout ?? ""
  }
  /** @param {string} text */
  type(text) {
    this.t(["send-keys", "-t", "oc", "-l", text])
  }
  /** @param {string[]} keys */
  keys(...keys) {
    this.t(["send-keys", "-t", "oc", ...keys])
  }
  /** @param {string} text */
  async prompt(text) {
    this.type(text)
    await sleep(200)
    this.keys("Enter")
  }
  /** @param {RegExp | string} what @param {number} [ms] */
  async waitFor(what, ms = 30000) {
    const t0 = Date.now()
    for (;;) {
      const s = this.capture()
      if (typeof what === "string" ? s.includes(what) : what.test(s)) return Date.now()
      if (Date.now() - t0 > ms) throw new Error(`pane never showed ${what}:\n${s}`)
      await sleep(20)
    }
  }
  ready(ms = 30000) {
    return this.waitFor("ctrl+p commands", ms)
  }

  /** Every record fake-ccd wrote. */
  records() {
    if (!existsSync(this.record)) return []
    return readFileSync(this.record, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l))
  }
  frames() {
    return this.records().filter((r) => r.frame && r.admitted === undefined)
  }
  /** @param {(r: any[]) => any} pred @param {number} [ms] */
  async until(pred, ms = 30000) {
    const t0 = Date.now()
    for (;;) {
      const v = pred(this.records())
      if (v) return v
      if (Date.now() - t0 > ms) throw new Error("timed out waiting for fake-ccd records")
      await sleep(50)
    }
  }
  stats() {
    try {
      return JSON.parse(readFileSync(join(this.sess, "stats.json"), "utf8"))
    } catch {
      return null
    }
  }
  acked() {
    return ackedFrom(this.frames().map((r) => r.frame))
  }

  /** Ends OpenCode (waiting for it to exit, so it never competes with the next run) and clears its data. */
  async reset() {
    this.t(["kill-server"])
    const pid = this.pid
    for (let i = 0; pid && i < 100 && alive(pid); i++) await sleep(50)
    if (pid && alive(pid)) process.kill(pid, "SIGKILL")
    for (const d of [join(this.home, ".local", "share", "opencode"), join(this.home, ".local", "state", "opencode")]) rmSync(d, { recursive: true, force: true })
    rmSync(this.record, { force: true })
    spawnSync("git", ["clean", "-qfdx"], { cwd: this.proj })
  }

  async close() {
    this.t(["kill-server"])
    this.stopCcd()
    this.mock?.kill("SIGKILL")
    rmSync(this.root, { recursive: true, force: true })
  }
}

/** @param {number} pid */
function alive(pid) {
  try {
    process.kill(pid, 0)
    return !/^\S+ \(.*\) Z/.test(readFileSync(`/proc/${pid}/stat`, "utf8"))
  } catch {
    return false
  }
}

/** Percentile with linear interpolation. @param {number[]} v @param {number} p */
export function pct(v, p) {
  const s = [...v].sort((a, b) => a - b)
  if (!s.length) return NaN
  const k = ((s.length - 1) * p) / 100
  const f = Math.floor(k)
  const c = Math.min(f + 1, s.length - 1)
  return s[f] + (s[c] - s[f]) * (k - f)
}
