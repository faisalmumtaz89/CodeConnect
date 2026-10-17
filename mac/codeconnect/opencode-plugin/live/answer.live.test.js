// Live: a real OpenCode TUI asks a real permission, fake-ccd.js answers it, and the plugin's frames are checked for
// shape and order only: the fake accepts whatever the plugin sends, so this is no oracle for the decision itself.
//   - once, always, reject: the card carries its context (the mode read from the TUI, tool ids, parent), the
//     stage frames arrive received -> sent -> result 200, OpenCode publishes the reply and the turn goes on (a
//     reject ends it: the tool part errors and the session goes idle, as OpenCode's processor breaks the turn);
//   - a reject with words at a root is refused before any call and remembered into the next snapshot;
//   - under --auto no card is raised: OpenCode's own reply is the only frame;
//   - a snapshot lists a pending card with the same context.
import { afterAll, beforeAll, describe, expect, test } from "bun:test"
import { rmSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { LIVE, Rig, sleep } from "./harness.js"

describe.skipIf(!LIVE)("live answer", () => {
  /** @type {Rig} */
  let rig
  beforeAll(async () => {
    rig = new Rig()
    await rig.start({ "*": "allow", bash: "ask" })
    await rig.startCcd()
    await rig.launch()
    await rig.ready(60000)
    await rig.reset()
  }, 90000)
  afterAll(async () => rig && (await rig.close()), 30000)

  /** @param {any} [a] what fake-ccd answers every card with; none removes the file */
  const answerWith = (a) => (a ? writeFileSync(join(rig.sess, "answer.json"), JSON.stringify(a)) : rmSync(join(rig.sess, "answer.json"), { force: true }))
  /** @param {any[]} recs @param {string} id */
  const stages = (recs, id) => recs.filter((r) => r.frame?.t === "answer" && r.frame.request_id === id).map(({ frame: { t, seq, ...rest } }) => rest)
  /** @param {string[]} [args] */
  const fresh = async (args) => {
    await rig.reset()
    await rig.launch({ args })
    await rig.ready()
    await rig.until((r) => r.some((x) => x.frame?.t === "settled"))
  }

  for (const reply of ["once", "always", "reject"]) {
    test(`${reply}: the card's context, the stages in order, OpenCode's reply, the turn ${reply === "reject" ? "ends" : "goes on"}`, async () => {
      answerWith({ reply })
      await fresh()
      await rig.prompt("cc:ask")
      const card = await rig.until((r) => r.find((x) => x.frame?.t === "ev" && x.frame.type === "permission.asked")?.frame.properties)
      expect(card.ctx).toMatchObject({ ok: true, mode: "normal", parent: null })
      expect(card.ctx.tool_ids).toContain("bash")
      expect(card.ctx.ms).toBeLessThanOrEqual(500)
      expect(card.anchor_input.command).toBe("echo approved-run")
      const got = await rig.until((r) => stages(r, card.id).some((s) => s.stage === "result") && stages(r, card.id))
      expect(got).toEqual([{ request_id: card.id, stage: "received" }, { request_id: card.id, stage: "sent" }, { request_id: card.id, stage: "result", http: 200 }])
      await rig.until((r) => r.some((x) => x.frame?.t === "ev" && x.frame.type === "permission.replied" && x.frame.properties.requestID === card.id && x.frame.properties.reply === reply))
      if (reply === "reject") await rig.until((r) => r.some((x) => x.frame?.t === "ev" && x.frame.type === "session.idle" && x.frame.properties.sessionID === card.sessionID))
      else await rig.waitFor("ask finished")
      await sleep(1000)
      const tool = rig.frames().find((x) => x.frame.t === "ev" && x.frame.type === "message.part.updated" && x.frame.properties.part?.type === "tool" && x.frame.properties.part.state?.status !== "running")?.frame.properties.part
      console.log(reply + ": stages", JSON.stringify(got), "ctx ms", card.ctx.ms, "tool", tool?.state?.status)
      if (reply === "reject") {
        expect(tool?.state?.status).toBe("error")
        // the turn ends on the rejection: no step starts after the rejected part, and the model's closing text never comes
        const parts = rig.frames().map((x) => x.frame).filter((x) => x.t === "ev" && x.type === "message.part.updated").map((x) => x.properties.part)
        const after = parts.slice(parts.findIndex((p) => p?.id === tool?.id && p.state?.status === "error") + 1)
        expect(after.filter((p) => p?.type === "step-start")).toEqual([])
        expect(parts.filter((p) => p?.type === "text" && p.text?.includes("ask finished"))).toEqual([])
      } else expect(tool?.state?.output).toContain("approved-run")
    }, 90000)
  }

  test("a reject with words at a root is refused before any call and remembered into the next snapshot", async () => {
    answerWith({ reply: "reject", message: "not that file" })
    await fresh()
    await rig.prompt("cc:ask")
    const card = await rig.until((r) => r.find((x) => x.frame?.t === "ev" && x.frame.type === "permission.asked")?.frame.properties)
    const got = await rig.until((r) => stages(r, card.id).some((s) => s.stage === "refused") && stages(r, card.id))
    expect(got).toEqual([{ request_id: card.id, stage: "received" }, { request_id: card.id, stage: "refused", reason: "words-at-root", detail: "a root session's reject takes no message" }])
    await sleep(500)
    expect(rig.capture()).toContain("Allow once")
    expect(rig.frames().some((x) => x.frame.t === "ev" && x.frame.type === "permission.replied")).toBe(false)
    rig.signalCcd("SIGUSR2")
    const begin = await rig.until((r) => r.find((x) => x.frame?.t === "sync_begin" && x.frame.commands)?.frame)
    expect(begin.commands).toEqual([{ request_id: card.id, stage: "refused", reason: "words-at-root" }])
    rig.keys("Enter")
    await rig.waitFor("ask finished")
  }, 90000)

  test("under --auto no card is raised; OpenCode's own reply is forwarded and the command runs", async () => {
    answerWith({ reply: "reject" })
    await fresh(["--auto"])
    await rig.prompt("cc:ask")
    await rig.waitFor("ask finished")
    await sleep(1500)
    const frames = rig.frames().map((x) => x.frame)
    const replied = frames.filter((x) => x.t === "ev" && x.type === "permission.replied")
    expect(replied.length).toBe(1)
    expect(replied[0].properties.reply).toBe("once")
    expect(frames.filter((x) => x.t === "ev" && x.type === "permission.asked")).toEqual([])
    expect(frames.filter((x) => x.t === "answer")).toEqual([])
    expect(frames.filter((x) => x.t === "sync_request")).toEqual([])
  }, 90000)

  test("a snapshot lists a pending card with its context", async () => {
    answerWith()
    await fresh()
    await rig.prompt("cc:ask")
    await rig.waitFor("Allow once")
    rig.signalCcd("SIGUSR2")
    const req = await rig.until((r) => r.find((x) => x.frame?.t === "sync_request")?.frame)
    expect(req.kind).toBe("permission")
    expect(req.request.ctx).toMatchObject({ ok: true, mode: "normal", parent: null, mcp: [] })
    expect(req.request.ctx.tool_ids).toContain("bash")
    expect(req.verdict).toEqual({ live: true, dead: null, listed: true, in_store: true })
    const card = rig.frames().find((x) => x.frame.t === "ev" && x.frame.type === "permission.asked")?.frame.properties
    expect({ ...card.ctx, ms: 0 }).toEqual({ ...req.request.ctx, ms: 0 })
    rig.keys("Enter")
    await rig.waitFor("ask finished")
  }, 90000)
})
