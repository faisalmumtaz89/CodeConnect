// Live bounds against a real OpenCode TUI:
//   - A-many: one bash turn writes 150 files of 60 KB (its user message carries 150 patches): the link stays up,
//     no frame is over 1 MiB, the queue stays under 4 MiB plus one page;
//   - B-w64: a session of many 60 KB writes, made headless, then opened with `-s` while the daemon acks nothing:
//     every page is at most 256 KiB, and the resync after a cut is incremental;
//   - flood: while a turn floods the bus, what the plugin costs OpenCode's main thread, measured inside OpenCode
//     against a control build that only samples the lag: its listeners' time per call and per run, the main thread's
//     lag beside the control's, and the keystroke echo pooled over every run; frames and queue stay inside the same
//     bounds.
import { afterAll, beforeAll, describe, expect, test } from "bun:test"
import { spawn } from "node:child_process"
import { LIVE, instrumented, lagBetween, lagProbe, pct, Rig, sleep } from "./harness.js"

const MiB = 1048576
const PAGE = 262144
const RUNS = Number(process.env.CC_OPENCODE_LIVE_RUNS || 5)
// Flood thresholds. Basis: 12 runs per arm on Linux (4 cores), OpenCode 1.18.34, each run a 20 s flood.
// - One listener call: measured 4.9-20.5 ms at most per run (garbage collection landing inside a call). 50 ms is
//   the point where a single stall becomes a visible hitch at the keyboard.
const LISTENER_MAX_MS = 50
// - All listener calls of one run (about 395 calls): measured 47-117 ms. 400 ms is 2% of the flood.
const LISTENER_TOTAL_MS = 400
// - Main-thread lag, median over runs of each run's 99th percentile: 41 ms without the plugin, 48 ms with it; two
//   groups of 5 runs from the same arm differ by up to 11 ms. The margin is that noise plus the measured cost.
const LAG_P99_MARGIN_MS = 20
// - Keystroke echo p50 pooled over every run: 31 ms without, 32 ms with; two groups of 5 runs from the same arm
//   differ by at most 3 ms (5,000 random splits). Total lag is printed but not asserted: OpenCode's own freezes
//   while it renders the long reply make it vary by over 4 s between runs of the same arm.
const ECHO_P50_MARGIN_MS = 4
const TURNS = Number(process.env.CC_OPENCODE_LIVE_TURNS || 200)

/** @param {any[]} recs */
const linkFrames = (recs) => recs.filter((r) => r.frame && r.admitted === undefined)
/** @param {any[]} recs */
const largest = (recs) => Math.max(0, ...linkFrames(recs).map((r) => r.bytes))
/** Admitted links. @param {any[]} recs */
const conns = (recs) => recs.filter((r) => r.admitted === true).length

describe.skipIf(!LIVE)("live bounds", () => {
  /** @type {Rig} */
  let rig
  beforeAll(async () => {
    rig = new Rig()
    await rig.start()
    await rig.startCcd()
    await rig.launch()
    await rig.ready(60000)
    await rig.prompt("cc:fast")
    await rig.waitFor("fast reply done", 60000)
    await rig.reset()
  }, 90000)
  afterAll(async () => rig && (await rig.close()), 30000)

  test("A-many: 150 x 60 KB files", async () => {
    await rig.launch({ plugin: instrumented() })
    await rig.ready()
    await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
    await rig.prompt("cc:many")
    await rig.waitFor("many finished", 60000)
    await sleep(2000)
    const recs = rig.records()
    const st = rig.stats()
    console.log("A-many:", JSON.stringify({ frames: linkFrames(recs).length, largestFrame: largest(recs), connections: conns(recs), plugin: { ...st, lag: undefined } }))
    expect(conns(recs)).toBe(1)
    expect(largest(recs)).toBeLessThanOrEqual(MiB)
    expect(st.drops).toBe(0)
    expect(st.maxQueue).toBeLessThanOrEqual(4 * MiB + PAGE)
  }, 120000)

  test(`B-w64: ${TURNS} turns of 60 KB writes, resynced from the start`, async () => {
    await rig.reset()
    rig.stopCcd()
    const serve = spawn(rig.bin, ["serve", "--port", "0", "--hostname", "127.0.0.1"], { cwd: rig.proj, env: rig.env(), stdio: ["ignore", "pipe", "pipe"] })
    let out = ""
    serve.stdout.on("data", (d) => (out += d))
    serve.stderr.on("data", (d) => (out += d))
    try {
      const t0 = Date.now()
      /** @type {RegExpExecArray | null} */
      let at
      while (!(at = /listening on (http:\/\/127\.0\.0\.1:\d+)/.exec(out))) {
        if (Date.now() - t0 > 30000) throw new Error("opencode serve did not start: " + out)
        await sleep(100)
      }
      const base = at[1]
      const q = `?directory=${encodeURIComponent(rig.proj)}`
      const sid = (await (await fetch(`${base}/session${q}`, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" })).json()).id
      const g0 = Date.now()
      for (let i = 0; i < TURNS; i++) {
        const r = await fetch(`${base}/session/${sid}/message${q}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ parts: [{ type: "text", text: "cc:w64" }] }) })
        await r.arrayBuffer()
      }
      console.log(`B-w64: generated ${TURNS} turns in ${Date.now() - g0} ms`)
      serve.kill()
      await sleep(1000)

      await rig.startCcd({ [sid]: { from: null, inclusive: false } })
      await rig.launch({ plugin: instrumented(), args: ["-s", sid] })
      await rig.ready(60000)
      const first = await rig.until((r) => r.find((x) => x.frame?.t === "sync_end" && x.frame.sync === 1)?.frame, 120000)
      await rig.until((r) => r.some((x) => x.frame?.t === "settled" && x.frame.sync === 1), 60000)
      const pages = linkFrames(rig.records()).filter((r) => r.frame.t === "sync_page").map((r) => r.bytes)
      rig.signalCcd("SIGUSR1")
      const second = await rig.until((r) => r.filter((x) => x.frame?.t === "sync_end").at(-1)?.frame.sync > 1 && r.filter((x) => x.frame?.t === "sync_end").at(-1).frame, 30000)
      await sleep(500)
      const recs = rig.records()
      const st = rig.stats()
      console.log("B-w64:", JSON.stringify({ first: { pages: first.pages, items: first.items, bytes: first.bytes, maxPage: Math.max(...pages) }, second: { pages: second.pages, items: second.items, bytes: second.bytes, lower: second.lower }, largestFrame: largest(recs), plugin: { ...st, lag: undefined } }))
      expect(first.pages).toBeGreaterThan(1)
      for (const b of pages) expect(b).toBeLessThanOrEqual(PAGE)
      expect(largest(recs)).toBeLessThanOrEqual(MiB)
      expect(st.drops).toBe(0)
      expect(st.maxQueue).toBeLessThanOrEqual(4 * MiB + PAGE)
      expect(second.bytes).toBeLessThan(first.bytes / 10)
    } finally {
      serve.kill("SIGKILL")
      rig.stopCcd()
      await rig.startCcd()
    }
  }, 900000)

  test(`flood: the plugin's own main-thread cost while a turn floods the bus (${RUNS} runs with it, ${RUNS} without)`, async () => {
    /** @type {Record<string, { p50: number, lat: number[], lost: number, lag: ReturnType<typeof lagBetween> }[]>} */
    const runs = { probe: [], instr: [] }
    /** @type {{ calls: number, ms: number, max: number }[]} */
    const listener = []
    /** @type {any[]} */
    const bounds = []
    const source = { probe: lagProbe(), instr: instrumented() }
    for (let i = 0; i < RUNS; i++)
      for (const arm of /** @type {const} */ (i % 2 ? ["instr", "probe"] : ["probe", "instr"])) {
        await rig.reset()
        await rig.launch({ plugin: source[arm] })
        await rig.ready()
        if (arm === "instr") await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
        await sleep(500)
        const { lat, lost, before, after } = await probe(rig, "cc:flood", 20000)
        runs[arm].push({ p50: pct(lat, 50), lat, lost, lag: lagBetween(before?.lag, after.lag) })
        if (arm === "instr") {
          listener.push({ calls: after.time.listenerCalls - (before?.time.listenerCalls ?? 0), ms: after.time.listenerMs - (before?.time.listenerMs ?? 0), max: after.time.listenerMaxMs })
          bounds.push({ largest: largest(rig.records()), drops: after.drops, maxQueue: after.maxQueue })
        }
      }
    const med = (/** @type {number[]} */ v) => pct(v, 50)
    const pooled = (/** @type {string} */ arm) => runs[arm].flatMap((r) => r.lat)
    const lagTotal = (/** @type {string} */ arm) => med(runs[arm].map((r) => r.lag.totalMs))
    const lagP99 = (/** @type {string} */ arm) => med(runs[arm].map((r) => r.lag.p99))
    console.log(
      "flood:",
      JSON.stringify({
        runs: RUNS,
        echo_p50_per_run: { without: runs.probe.map((r) => r.p50), with: runs.instr.map((r) => r.p50) },
        echo_pooled: {
          without: { p50: pct(pooled("probe"), 50), p95: pct(pooled("probe"), 95), n: pooled("probe").length },
          with: { p50: pct(pooled("instr"), 50), p95: pct(pooled("instr"), 95), n: pooled("instr").length },
        },
        lost: { without: runs.probe.reduce((n, r) => n + r.lost, 0), with: runs.instr.reduce((n, r) => n + r.lost, 0) },
        lag_total_ms_median: { without: lagTotal("probe"), with: lagTotal("instr") },
        lag_p99_ms_median: { without: lagP99("probe"), with: lagP99("instr") },
        listener,
        bounds,
      }),
    )
    // What the plugin itself spends on the main thread, measured inside OpenCode, not inferred from echo noise.
    for (const l of listener) {
      expect(l.max).toBeLessThanOrEqual(LISTENER_MAX_MS)
      expect(l.ms).toBeLessThanOrEqual(LISTENER_TOTAL_MS)
    }
    expect(lagP99("instr")).toBeLessThanOrEqual(lagP99("probe") + LAG_P99_MARGIN_MS)
    // Keystroke echo, pooled over every run: only a margin above the measured run-to-run noise is asserted.
    expect(pct(pooled("instr"), 50)).toBeLessThanOrEqual(pct(pooled("probe"), 50) + ECHO_P50_MARGIN_MS)
    for (const b of bounds) {
      expect(b.largest).toBeLessThanOrEqual(MiB)
      expect(b.drops).toBe(0)
      expect(b.maxQueue).toBeLessThanOrEqual(4 * MiB + PAGE)
    }
  }, 2400000)
})

/**
 * Fixed-clock keystroke probe, started once the turn is running: tick k is due at t0 + 250k ms whatever happened
 * before; it types one `Q` into the prompt and its latency is the first capture showing it, measured from the tick
 * (not from the send), so a stall of the TUI counts against every keystroke it delays. A Q not shown within 5 s is
 * counted as lost and the count is re-read.
 * @param {Rig} rig
 * @param {string} prompt
 * @param {number} ms
 */
async function probe(rig, prompt, ms) {
  await rig.prompt(prompt)
  await rig.waitFor("esc interrupt")
  await sleep(100)
  const before = rig.stats()
  const countQ = () => (rig.capture().match(/Q/g) ?? []).length
  let seen = countQ()
  let lost = 0
  /** @type {number[]} */
  const lat = []
  const t0 = Date.now()
  for (let k = 0; Date.now() - t0 < ms; k++) {
    const due = t0 + k * 250
    const wait = due - Date.now()
    if (wait > 0) await sleep(wait)
    rig.type("Q")
    for (;;) {
      const c = countQ()
      if (c > seen) {
        lat.push(Date.now() - due)
        seen = c
        break
      }
      if (Date.now() - due > 5000) {
        lost++
        seen = countQ()
        break
      }
      await sleep(3)
    }
    if (k % 20 === 19) {
      for (let j = 0; j < 25; j++) rig.keys("BSpace")
      await sleep(200)
      seen = countQ()
    }
  }
  await sleep(300)
  return { lat, lost, before, after: rig.stats() }
}
