// Live: a real OpenCode TUI with the plugin, against fake-ccd.js and the mock model.
//   - a plugin whose forward() throws still leaves the reply on the keyboard's screen;
//   - with the daemon SIGSTOPped, the prompt is drawn within 5 s of the agent starting;
//   - a request pending at a snapshot is listed with a live verdict, and is gone once answered at the keyboard;
//   - a link cut, and a daemon kill and restart, mid-turn, end with the same messages and parts as an uncut run.
import { afterAll, beforeAll, describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { LIVE, Rig, sleep, variant } from "./harness.js"

/** Milliseconds since the process started, to 10 ms (/proc ticks), measured against the monotonic uptime. */
function agentAgeMs(/** @type {number} */ pid) {
  const stat = readFileSync(`/proc/${pid}/stat`, "utf8")
  const ticks = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19])
  const uptime = Number(readFileSync("/proc/uptime", "utf8").split(" ")[0])
  return (uptime - ticks / 100) * 1000
}

/**
 * The terminal state of every message and part the daemon was told about, live or by snapshot, per session, in id
 * order, without ids: what a reader that keeps only finished state ends up with.
 * @param {any[]} frames
 */
export function projection(frames) {
  /** @type {Map<string, any>} */ const msgs = new Map()
  /** @type {Map<string, any>} */ const parts = new Map()
  const info = (/** @type {any} */ i) => {
    if (i && (i.role === "user" || i.time?.completed)) msgs.set(i.id, { sid: i.sessionID, role: i.role })
  }
  const part = (/** @type {any} */ p) => {
    if (!p) return
    const done =
      p.type === "tool" ? p.state?.status === "completed" || p.state?.status === "error" : p.type === "text" || p.type === "reasoning" ? !("time" in p) || p.time?.end != null : true
    if (!done || p.type === "step-start") return
    const v = p.type === "tool" ? `${p.tool}/${p.state.status}/${p.state.output ?? p.state.error ?? ""}` : p.type === "text" || p.type === "reasoning" ? p.text : ""
    parts.set(p.id, { mid: p.messageID, line: p.type + ":" + v })
  }
  for (const f of frames) {
    if (f.t === "ev" && f.type === "message.updated") info(f.properties.info)
    if (f.t === "ev" && f.type === "message.part.updated") part(f.properties.part)
    if (f.t === "sync_page")
      for (const it of f.items) {
        info(it.info)
        for (const p of it.parts ?? (it.part ? [it.part] : [])) part(p)
      }
  }
  return [...msgs.entries()]
    .sort(([a], [b]) => (a < b ? -1 : 1))
    .map(([id, m]) => m.role + " " + [...parts.entries()].filter(([, p]) => p.mid === id).sort(([a], [b]) => (a < b ? -1 : 1)).map(([, p]) => p.line).join(" | "))
}

describe.skipIf(!LIVE)("live link", () => {
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

  test("a plugin whose forward() throws leaves the reply visible", async () => {
    const out = /** @type {Record<string, number>} */ ({})
    const evs = /** @type {Record<string, number>} */ ({})
    for (const [name, plugin] of [
      ["throwing", variant([["  function forward(type, event) {\n", '    throw new Error("forward fails")\n']])],
      ["real", "real"],
      ["none", "none"],
    ]) {
      await rig.reset()
      await rig.launch({ plugin })
      await rig.ready()
      await rig.prompt("cc:fast")
      const t0 = Date.now()
      const seen = await rig.waitFor("fast reply done", 20000)
      out[name] = seen - t0
      evs[name] = rig.frames().filter((r) => r.frame.t === "ev").length
    }
    console.log("Enter -> reply visible, ms:", JSON.stringify(out), "event frames:", JSON.stringify(evs))
    expect(evs.throwing).toBe(0)
    expect(evs.real).toBeGreaterThan(0)
  }, 120000)

  test("with the daemon stopped, the prompt is drawn within 5 s of the agent starting (or as fast as without the plugin)", async () => {
    const rows = []
    for (const plugin of ["none", "real", "none", "real", "none", "real"]) {
      await rig.reset()
      if (plugin === "real") rig.signalCcd("SIGSTOP")
      await rig.launch({ plugin })
      const age0 = agentAgeMs(rig.pid)
      const t0 = Date.now()
      const seen = await rig.ready(20000)
      rows.push({ plugin, ms: Math.round(seen - t0 + age0) })
      if (plugin === "real") {
        rig.signalCcd("SIGCONT")
        await rig.until((r) => r.some((x) => x.frame?.t === "settled"), 20000)
      }
    }
    console.log("agent start -> prompt drawn, ms:", JSON.stringify(rows))
    // OpenCode's own start-up is the floor: on a machine where it alone takes over 5 s, the stopped daemon may add
    // at most a second to it.
    const base = rows.filter((r) => r.plugin === "none").map((r) => r.ms).sort((a, b) => a - b)[1]
    for (const r of rows) if (r.plugin === "real") expect(r.ms).toBeLessThanOrEqual(Math.max(5000, base + 1000))
  }, 120000)

  test("a pending request is listed live at a snapshot and absent once answered", async () => {
    rig.config({ "*": "allow", bash: "ask" })
    try {
      await rig.reset()
      await rig.launch()
      await rig.ready()
      await rig.prompt("cc:ask")
      await rig.waitFor("Allow once")
      rig.signalCcd("SIGUSR2")
      const req = await rig.until((r) => r.find((x) => x.frame?.t === "sync_request")?.frame)
      expect(req.kind).toBe("permission")
      expect(req.request.metadata.command).toBe("echo approved-run")
      expect(req.request.anchor_input.command).toBe("echo approved-run")
      expect(req.verdict).toEqual({ live: true, dead: null, listed: true, in_store: true })
      rig.keys("Enter")
      await rig.waitFor("ask finished")
      await rig.until((r) => r.some((x) => x.frame?.t === "ev" && x.frame.type === "permission.replied"))
      const idleSync = await rig.until((r) => r.find((x) => x.frame?.t === "sync_end" && x.frame.sync > 2)?.frame, 10000)
      const after = rig.frames().filter((x) => x.frame.t === "sync_request" && x.frame.sync === idleSync.sync)
      expect(after).toEqual([])
    } finally {
      rig.config({ "*": "allow", bash: "allow" })
    }
  }, 60000)

  test("a link cut and a daemon kill+restart mid-turn end like the uncut run", async () => {
    await rig.reset()
    await rig.launch()
    await rig.ready()
    await rig.prompt("cc:steps")
    await rig.waitFor("All steps finished.", 30000)
    await sleep(1500)
    const uncut = projection(rig.frames().map((r) => r.frame))

    await rig.reset()
    await rig.launch()
    await rig.ready()
    await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
    await rig.prompt("cc:steps")
    await sleep(1000)
    rig.signalCcd("SIGUSR1")
    await sleep(2000)
    rig.stopCcd()
    await sleep(800)
    await rig.startCcd()
    await rig.waitFor("All steps finished.", 30000)
    await sleep(2500)
    const recs = rig.records()
    const cut = projection(recs.filter((r) => r.frame && r.admitted === undefined).map((r) => r.frame))
    const welcomes = recs.filter((r) => r.sent?.type === "opencode_welcome")
    console.log("cut and restart: welcomes", welcomes.length, "acked at restart", JSON.stringify(welcomes.at(-1)?.sent.acked), "rows", cut.length)
    expect(welcomes.length).toBeGreaterThanOrEqual(3)
    expect(cut).toEqual(uncut)
    expect(uncut.length).toBeGreaterThanOrEqual(4)
  }, 90000)
})
