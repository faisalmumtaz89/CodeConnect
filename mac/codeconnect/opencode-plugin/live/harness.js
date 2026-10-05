// Live-test rig: a real OpenCode TUI in a private tmux server, the scripted mock model, and fake-ccd.js, all under
// one temporary directory that is removed afterwards.
//
// Gate: nothing runs unless CC_OPENCODE_LIVE=1. With the flag set, a missing OpenCode binary, or one whose
// `--version` is not 1.18.34, is a failure, not a skip. The binary is CC_OPENCODE_BIN, else `opencode` on PATH.
//
// Disk: OpenCode unpacks its native libraries into TMPDIR at every start and leaves them there (about 19 MB a
// launch), and when the disk fills its database writes fail and a turn stops mid-way. So every OpenCode here gets
// TMPDIR inside the rig, cleared after each run and removed with the rig, also on a failed or interrupted run; a
// rig left by a killed run is swept at the next start, and a rig refuses to start with less than 512 MiB free (a rig peaks near 200 MB).
// Every process a rig starts outside tmux (the mock model, fake-ccd, `opencode serve`), and the pane's OpenCode, is
// recorded in the rig directory with its start time, so the sweep can stop what a killed run left running.
import { spawn, spawnSync } from "node:child_process"
import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, renameSync, rmSync, statfsSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { delimiter, join } from "node:path"
import { fileURLToPath } from "node:url"
import { ackedFrom } from "./fake-ccd.js"
import { alive, birth, killIfSame } from "./proc.js"

export const LIVE = process.env.CC_OPENCODE_LIVE === "1"
if (!LIVE) console.error("SKIP live OpenCode tests: set CC_OPENCODE_LIVE=1 (and CC_OPENCODE_BIN) to run them")

const PLUGIN = fileURLToPath(new URL("../codeconnect-opencode.js", import.meta.url))
const FAKE_CCD = fileURLToPath(new URL("./fake-ccd.js", import.meta.url))
const MOCK = fileURLToPath(new URL("./mock-model.js", import.meta.url))
export const NONCE = "0123456789abcdef0123456789abcdef"
export const sleep = (/** @type {number} */ ms) => new Promise((r) => setTimeout(r, ms))

export const OPENCODE_VERSION = "1.18.34"
const FREE_MIN = 512 * 1048576
const PREFIX = "cc-oc-live-"
/** In each rig directory: one `{pid, start}` line per process the rig started. */
const PIDS = "pids.jsonl"
/** Where tmux keeps its server sockets; a killed server can leave its socket file behind. */
const SOCKETS = join(process.env.TMUX_TMPDIR ?? "/tmp", `tmux-${process.getuid?.() ?? 0}`)

/** The OpenCode binary, checked to answer `--version` with exactly OPENCODE_VERSION. */
export function opencodeBin() {
  let bin = process.env.CC_OPENCODE_BIN
  if (bin && !existsSync(bin)) throw new Error(`CC_OPENCODE_LIVE=1 but CC_OPENCODE_BIN=${bin} does not exist`)
  for (const d of bin ? [] : (process.env.PATH ?? "").split(delimiter)) if (d && existsSync(join(d, "opencode"))) (bin ??= join(d, "opencode"))
  if (!bin) throw new Error("CC_OPENCODE_LIVE=1 but no OpenCode binary was found: set CC_OPENCODE_BIN rather than pass vacuously")
  const scratch = mkdtempSync(join(tmpdir(), `${PREFIX}${process.pid}-version-`))
  try {
    const r = spawnSync(bin, ["--version"], { encoding: "utf8", timeout: 30000, env: { PATH: process.env.PATH ?? "", HOME: scratch, TMPDIR: scratch } })
    const got = (r.stdout ?? "").trim()
    if (got !== OPENCODE_VERSION) throw new Error(`CC_OPENCODE_LIVE=1 needs OpenCode ${OPENCODE_VERSION}, but ${bin} --version answered ${JSON.stringify(got || r.stderr || r.error?.message)}`)
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
  return bin
}

/** Rigs of this process, removed on exit even when a test failed or the run was interrupted. */
const open = new Set()
function cleanupAll() {
  for (const rig of open) rig.closeSync()
}
process.on("exit", cleanupAll)
for (const sig of /** @type {const} */ (["SIGINT", "SIGTERM", "SIGHUP"]))
  process.once(sig, () => {
    cleanupAll()
    process.kill(process.pid, sig)
  })

/**
 * Removes what a killed run left: its tmux servers (and so its OpenCode) and their socket files, every recorded
 * process still running with its recorded start time, and its rig directories.
 */
function sweep() {
  const dead = (/** @type {string} */ name) => {
    const pid = Number(name.slice(PREFIX.length).split("-")[0])
    return Number.isInteger(pid) && pid > 0 && pid !== process.pid && !alive(pid)
  }
  try {
    for (const n of readdirSync(SOCKETS))
      if (n.startsWith(PREFIX) && dead(n)) {
        spawnSync("tmux", ["-L", n, "kill-server"])
        rmSync(join(SOCKETS, n), { force: true })
      }
  } catch {}
  for (const n of readdirSync(tmpdir())) {
    if (!n.startsWith(PREFIX) || !dead(n)) continue
    let recorded = ""
    try {
      recorded = readFileSync(join(tmpdir(), n, PIDS), "utf8")
    } catch {}
    for (const l of recorded.split("\n").filter(Boolean)) {
      try {
        const { pid, start } = JSON.parse(l)
        killIfSame(pid, start)
      } catch {}
    }
    rmSync(join(tmpdir(), n), { recursive: true, force: true })
  }
}

const freeBytes = (/** @type {string} */ dir) => {
  const f = statfsSync(dir)
  return f.bavail * f.bsize
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

/**
 * Main-thread lag sampler: a 20 ms interval timer whose lateness is counted in a 1 ms histogram (`lag.hist`, last
 * bucket 1000+ ms), with the total lateness and the sample count. Cumulative; the tests diff two readings.
 */
const LAG_SAMPLER = `const __lag = { samples: 0, totalMs: 0, maxMs: 0, hist: new Array(1001).fill(0) }
let __last = performance.now()
setInterval(() => { const n = performance.now(); const l = Math.max(0, n - __last - 20); __last = n; __lag.samples++; __lag.totalMs += l; __lag.maxMs = Math.max(__lag.maxMs, l); __lag.hist[Math.min(1000, Math.floor(l))]++ }, 20).unref?.()
`
// Written to a temporary file and renamed over stats.json, so a reader never sees a partial file.
const STATS_WRITER = `setInterval(() => { try { const f = fileURLToPath(new URL("./stats.json", import.meta.url)); __w(f + ".tmp", JSON.stringify(__st)); __r(f + ".tmp", f) } catch {} }, 200).unref?.()
`

/**
 * A build that records the queue and frame bounds it reached, its main-thread time and the main thread's lag into
 * `<session dir>/stats.json`. Main-thread time is counted twice over: per listener call, and per synchronous slice of
 * a snapshot. A slice runs from the start of runSync, or from the resumption after a server read, a socket drain or
 * the yield between messages (the only waits in a snapshot that let the event loop run), to the next such wait or to
 * the snapshot's end; it holds everything runSync, message(), prepare() and the page building do in between.
 */
export const instrumented = (source = readFileSync(PLUGIN, "utf8")) =>
  variant(
    [
      [
        'import { fileURLToPath } from "node:url"\n',
        `import { renameSync as __r, writeFileSync as __w } from "node:fs"
const __st = { maxQueue: 0, maxFrame: 0, maxPage: 0, drops: 0, frames: 0 }
const __q = (/** @type {any} */ c, /** @type {number} */ extra) => { __st.maxQueue = Math.max(__st.maxQueue, (c?.writableLength ?? 0) + extra) }
const __t = { listenerCalls: 0, listenerMs: 0, listenerMaxMs: 0, syncSlices: 0, syncMs: 0, syncMaxMs: 0 }
let __s0 = 0
const __on = () => { __s0 = performance.now() }
const __off = () => { if (!__s0) return; const d = performance.now() - __s0; __s0 = 0; __t.syncSlices++; __t.syncMs += d; __t.syncMaxMs = Math.max(__t.syncMaxMs, d) }
${LAG_SAMPLER}Object.assign(__st, { time: __t, lag: __lag })
${STATS_WRITER}`,
      ],
      ["  function enqueue(e, s, card = false) {\n    const c = sock\n", "    __st.frames++; __st.maxFrame = Math.max(__st.maxFrame, e.bytes); __q(c, heldBytes + e.bytes)\n"],
      ["  function overflow(c) {\n", "    __st.drops++\n"],
      [
        "  function forward(type, event) {\n",
        "    const __t0 = performance.now()\n    try {\n      return __forward(type, event)\n    } finally {\n      const d = performance.now() - __t0\n      __t.listenerCalls++\n      __t.listenerMs += d\n      __t.listenerMaxMs = Math.max(__t.listenerMaxMs, d)\n    }\n  }\n  function __forward(type, event) {\n",
      ],
      ["      pages++\n", "      __st.maxPage = Math.max(__st.maxPage, utf8(line)); __q(c, utf8(line) + heldBytes)\n"],
      ["const call = async (f) => {\n", "  const __p = __call(f)\n  __off()\n  try {\n    return await __p\n  } finally {\n    __on()\n  }\n}\nconst __call = async (/** @type {() => Promise<any>} */ f) => {\n"],
      ["const yieldToLoop = () => ", "(__off(), __yield().finally(__on))\nconst __yield = () => "],
      ["  const drained = (c) =>\n", "    (__off(), __drained(c).finally(__on))\n  const __drained = (/** @type {Socket} */ c) =>\n"],
      [
        "  async function runSync(c, t) {\n",
        "    __on()\n    try {\n      return await __runSync(c, t)\n    } finally {\n      __off()\n    }\n  }\n  async function __runSync(/** @type {Socket} */ c, /** @type {Trigger} */ t) {\n",
      ],
    ],
    source,
  )

/** The control for `instrumented()`: the same lag sampler and stats file, and no listener, link or snapshot. */
export const lagProbe = () => `import { renameSync as __r, writeFileSync as __w } from "node:fs"
import { fileURLToPath } from "node:url"
const __st = {}
${LAG_SAMPLER}Object.assign(__st, { lag: __lag })
${STATS_WRITER}export default { id: "codeconnect", tui: async () => {} }
`

/**
 * Lag between two cumulative readings: samples, total and per-sample percentiles, in ms.
 * @param {any} a @param {any} b
 */
export function lagBetween(a, b) {
  const hist = b.hist.map((/** @type {number} */ n, /** @type {number} */ i) => n - a.hist[i])
  const samples = b.samples - a.samples
  const at = (/** @type {number} */ q) => {
    let seen = 0
    for (let i = 0; i < hist.length; i++) if ((seen += hist[i]) >= q * samples) return i
    return hist.length - 1
  }
  return { samples, totalMs: b.totalMs - a.totalMs, p50: at(0.5), p99: at(0.99), p999: at(0.999) }
}

let rigs = 0
export class Rig {
  constructor() {
    this.bin = opencodeBin()
    sweep()
    const free = freeBytes(tmpdir())
    if (free < FREE_MIN)
      throw new Error(`only ${Math.round(free / 1048576)} MiB free in ${tmpdir()}: the live tests need 512 MiB, because OpenCode stops a turn when its database writes fail`)
    this.root = mkdtempSync(join(tmpdir(), `${PREFIX}${process.pid}-`))
    open.add(this)
    this.home = join(this.root, "home")
    this.proj = join(this.root, "proj")
    this.sess = join(this.root, "sess")
    this.socket = join(this.root, "ccd.sock")
    this.record = join(this.root, "record.jsonl")
    this.tmp = join(this.root, "tmp")
    this.server = `${PREFIX}${process.pid}-${++rigs}`
    this.tmux = ["-L", this.server, "-f", "/dev/null"]
    /** @type {import("node:child_process").ChildProcess | null} */
    this.ccd = null
    /** @type {import("node:child_process").ChildProcess | null} */
    this.mock = null
    /** @type {Map<number, { sec: number, usec: number }>} pid -> start time of every running process spawned here */
    this.children = new Map()
    this.port = 0
    this.pid = 0
    this.pidStart = { sec: 0, usec: 0 }
    this.launchedAt = 0
    for (const d of [this.home, this.proj, this.sess, this.tmp]) mkdirSync(d, { recursive: true })
    writeFileSync(join(this.proj, "a.txt"), "hello\n")
    spawnSync("git", ["init", "-q"], { cwd: this.proj })
    spawnSync("git", ["-c", "user.email=t@example.invalid", "-c", "user.name=t", "commit", "-qm", "init", "--allow-empty"], { cwd: this.proj })
  }

  /**
   * Starts a process outside tmux, recorded with its start time in the rig directory and stopped by closeSync, by the
   * exit and signal cleanup, and by the next run's sweep.
   * @param {string} file @param {string[]} args @param {import("node:child_process").SpawnOptions} opts
   */
  spawn(file, args, opts) {
    const p = spawn(file, args, opts)
    if (p.pid) this.children.set(p.pid, this.recordPid(p.pid))
    p.once("exit", () => p.pid && this.children.delete(p.pid))
    return p
  }

  /** Appends `pid` and its start time to the rig's record, and returns the start time. @param {number} pid */
  recordPid(pid) {
    try {
      const start = birth(pid)
      appendFileSync(join(this.root, PIDS), JSON.stringify({ pid, start }) + "\n")
      return start
    } catch {
      return { sec: 0, usec: 0 }
    }
  }

  /** The mock model runs in its own process, so a busy test loop cannot slow its stream. */
  async start(permission = { "*": "allow", bash: "allow" }) {
    const p = this.spawn(process.execPath, [MOCK], { stdio: ["ignore", "pipe", "inherit"] })
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
    const env = { HOME: this.home, TMPDIR: this.tmp, PATH: process.env.PATH ?? "", TERM: "xterm-256color", OPENCODE_DISABLE_AUTOUPDATE: "1", OPENCODE_DISABLE_MODELS_FETCH: "1", NO_PROXY: "127.0.0.1,localhost", no_proxy: "127.0.0.1,localhost" }
    return env
  }

  /** @param {any} [acked] the first welcome's acked roots, used while nothing is recorded */
  async startCcd(acked) {
    const args = [FAKE_CCD, this.socket, this.sess, this.proj, this.record, ...(acked ? [JSON.stringify(acked)] : [])]
    const p = this.spawn(process.execPath, args, { stdio: ["ignore", "pipe", "inherit"] })
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
    rmSync(join(this.sess, "stats.json.tmp"), { force: true })
    if (plugin === "none") writeFileSync(join(this.sess, "tui.json"), JSON.stringify({ plugin: [] }))
    else {
      writeFileSync(join(this.sess, "codeconnect-opencode.js"), plugin === "real" ? readFileSync(PLUGIN) : plugin)
      writeFileSync(join(this.sess, "tui.json"), JSON.stringify({ plugin: [["./codeconnect-opencode.js", { socket: this.socket, nonce: NONCE }]] }))
    }
    const env = Object.entries({ ...this.env(), OPENCODE_TUI_CONFIG: join(this.sess, "tui.json") }).map(([k, v]) => `${k}=${v}`)
    this.t(["kill-server"])
    this.launchedAt = Date.now()
    const r = this.t(["new-session", "-d", "-s", "oc", "-x", "160", "-y", "50", "-c", this.proj, "--", "env", "-i", ...env, this.bin, ...(o.args ?? [])])
    if (r.status !== 0) throw new Error("tmux: " + r.stderr)
    this.pid = Number(this.t(["display", "-p", "-t", "oc", "#{pane_pid}"]).stdout.trim())
    this.pidStart = this.recordPid(this.pid)
    if (o.agent !== false) this.writeAgent()
    return this.pid
  }

  writeAgent() {
    const tmp = join(this.sess, "agent.json.tmp")
    writeFileSync(tmp, JSON.stringify({ pid: this.pid, start: this.pidStart }), { flag: "wx", mode: 0o600 })
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
      if (Date.now() - t0 > ms) throw new Error(`pane never showed ${what}:\n${s}\n${this.diagnosis()}`)
      await sleep(20)
    }
  }
  /** Free disk and OpenCode's own logged errors, for a failure message. */
  diagnosis() {
    const lines = [`free in ${tmpdir()}: ${Math.round(freeBytes(tmpdir()) / 1048576)} MiB`]
    const dir = join(this.home, ".local", "share", "opencode", "log")
    try {
      for (const f of readdirSync(dir)) lines.push(...readFileSync(join(dir, f), "utf8").split("\n").filter((l) => l.includes("level=ERROR")).slice(-5).map((l) => l.slice(0, 300)))
    } catch {}
    return lines.join("\n")
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
  /** What the instrumented plugin last wrote; a missing or unreadable file is an error, never a zero. */
  stats() {
    const f = join(this.sess, "stats.json")
    try {
      return JSON.parse(readFileSync(f, "utf8"))
    } catch (e) {
      throw new Error(`no readable ${f}: ${/** @type {Error} */ (e).message}`)
    }
  }
  acked() {
    return ackedFrom(this.frames().map((r) => r.frame))
  }

  /** Ends OpenCode (waiting for it to exit, so it never competes with the next run) and clears its data. */
  async reset() {
    this.t(["kill-server"])
    const pid = this.pid
    for (let i = 0; pid && i < 100 && this.running(pid); i++) await sleep(50)
    if (pid) killIfSame(pid, this.pidStart)
    for (const d of [join(this.home, ".local", "share", "opencode"), join(this.home, ".local", "state", "opencode"), this.tmp]) rmSync(d, { recursive: true, force: true })
    mkdirSync(this.tmp)
    rmSync(this.record, { force: true })
    spawnSync("git", ["clean", "-qfdx"], { cwd: this.proj })
  }

  async close() {
    this.t(["kill-server"])
    for (let i = 0; this.pid && i < 100 && this.running(this.pid); i++) await sleep(50)
    this.closeSync()
  }

  /** True while `pid` is still the pane's OpenCode, not a process that reuses its pid. @param {number} pid */
  running(pid) {
    try {
      return alive(pid) && birth(pid).sec === this.pidStart.sec
    } catch {
      return false
    }
  }

  /** Everything this rig started is stopped and its directory removed. Safe to call twice. */
  closeSync() {
    this.t(["kill-server"])
    rmSync(join(SOCKETS, this.server), { force: true })
    if (this.pid) killIfSame(this.pid, this.pidStart)
    for (const [pid, start] of this.children) killIfSame(pid, start)
    this.children.clear()
    this.ccd = null
    this.mock = null
    rmSync(this.root, { recursive: true, force: true })
    open.delete(this)
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
