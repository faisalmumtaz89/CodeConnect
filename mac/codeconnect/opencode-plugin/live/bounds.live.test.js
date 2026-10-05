// Live bounds against a real OpenCode TUI:
//   - A-many: one bash turn writes 150 files of 60 KB (its user message carries 150 patches): the link stays up,
//     no frame is over 1 MiB, the queue stays under 4 MiB plus one page;
//   - B-w64: a session of many 60 KB writes, made headless, then opened with `-s` while the daemon acks nothing:
//     every page is at most 256 KiB, and the resync after a cut is incremental;
//   - flood: keystroke echo p50 with the plugin is within 2 ms of the same OpenCode without it, over at least five
//     runs each, while a turn floods the bus; frames and queue stay inside the same bounds.
import { afterAll, beforeAll, describe, expect, test } from "bun:test"
import { spawn } from "node:child_process"
import { LIVE, instrumented, pct, Rig, sleep } from "./harness.js"

const MiB = 1048576
const PAGE = 262144
const RUNS = Number(process.env.CC_OPENCODE_LIVE_RUNS || 5)
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
    rig = await new Rig().start()
    await rig.startCcd()
    await rig.launch()
    await rig.ready(60000)
    await rig.prompt("cc:fast")
    await rig.waitFor("fast reply done", 60000)
    await rig.reset()
  }, 90000)
  afterAll(async () => rig && (await rig.close()))

  test("A-many: 150 x 60 KB files", async () => {
    await rig.launch({ plugin: instrumented() })
    await rig.ready()
    await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
    await rig.prompt("cc:many")
    await rig.waitFor("many finished", 60000)
    await sleep(2000)
    const recs = rig.records()
    const st = rig.stats()
    console.log("A-many:", JSON.stringify({ frames: linkFrames(recs).length, largestFrame: largest(recs), connections: conns(recs), plugin: st }))
    expect(conns(recs)).toBe(1)
    expect(largest(recs)).toBeLessThanOrEqual(MiB)
    expect(st.drops).toBe(0)
    expect(st.maxQueue).toBeLessThanOrEqual(4 * MiB + PAGE)
  }, 120000)

  test(`B-w64: ${TURNS} turns of 60 KB writes, resynced from the start`, async () => {
    await rig.reset()
    rig.stopCcd()
    const port = 20000 + (process.pid % 20000)
    const serve = spawn(rig.bin, ["serve", "--port", String(port), "--hostname", "127.0.0.1"], { cwd: rig.proj, env: rig.env(), stdio: ["ignore", "pipe", "pipe"] })
    let out = ""
    serve.stdout.on("data", (d) => (out += d))
    serve.stderr.on("data", (d) => (out += d))
    try {
      const t0 = Date.now()
      while (!out.includes("listening")) {
        if (Date.now() - t0 > 30000) throw new Error("opencode serve did not start: " + out)
        await sleep(100)
      }
      const base = `http://127.0.0.1:${port}`
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
      console.log("B-w64:", JSON.stringify({ first: { pages: first.pages, items: first.items, bytes: first.bytes, maxPage: Math.max(...pages) }, second: { pages: second.pages, items: second.items, bytes: second.bytes, lower: second.lower }, largestFrame: largest(recs), plugin: st }))
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

  test(`flood: keystroke echo p50 within 2 ms of no plugin (${RUNS} runs each)`, async () => {
    /** @type {Record<string, number[]>} */
    const p50 = { none: [], real: [] }
    /** @type {Record<string, number[]>} */
    const all = { none: [], real: [] }
    const frames = []
    const lostBy = { none: 0, real: 0 }
    for (let i = 0; i < RUNS; i++)
      for (const plugin of i % 2 ? ["real", "none"] : ["none", "real"]) {
        await rig.reset()
        await rig.launch({ plugin })
        await rig.ready()
        if (plugin === "real") await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
        const { lat, lost } = await probe(rig, "cc:flood", 20000)
        p50[plugin].push(pct(lat, 50))
        all[plugin].push(...lat)
        lostBy[plugin] += lost
        if (plugin === "real") frames.push(largest(rig.records()))
      }
    await rig.reset()
    await rig.launch({ plugin: instrumented() })
    await rig.ready()
    await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
    await rig.prompt("cc:flood")
    await rig.waitFor("Burst 3.", 30000)
    await sleep(8000)
    const st = rig.stats()
    const med = (/** @type {number[]} */ v) => pct(v, 50)
    console.log(
      "keystroke echo:",
      JSON.stringify({
        runs: RUNS,
        p50_per_run: p50,
        median_p50: { none: med(p50.none), real: med(p50.real) },
        pooled: { none: { p50: pct(all.none, 50), p95: pct(all.none, 95), n: all.none.length }, real: { p50: pct(all.real, 50), p95: pct(all.real, 95), n: all.real.length } },
        lost: lostBy,
        largestFrame: Math.max(...frames),
        instrumented: st,
      }),
    )
    expect(med(p50.real)).toBeLessThanOrEqual(med(p50.none) + 2)
    expect(Math.max(...frames)).toBeLessThanOrEqual(MiB)
    expect(st.drops).toBe(0)
    expect(st.maxQueue).toBeLessThanOrEqual(4 * MiB + PAGE)
  }, 1200000)
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
  return { lat, lost }
}
