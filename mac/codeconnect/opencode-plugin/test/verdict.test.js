// The liveness rule against s6-liveness: each case's events are replayed into a fresh per-activation EpochLog, and at
// every judged snapshot the verdict is computed from the snapshot's server-side facts and that log, once by the rule
// directly and once by the plugin's own judge() reading those facts from a fake server (doom-loop asks included). No request the
// oracle found dead may be judged live; the only disagreements are the fixture's named, designed refusals.
import { describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { EPOCH_EVENTS_CAP, EpochLog, judge, verdict } from "../codeconnect-opencode.js"
import { fixture } from "./helpers.js"

const s6 = JSON.parse(readFileSync(fixture("s6-liveness-1.18.34.json"), "utf8"))

/** Rebuilds the bus properties the log reads from one probe record. @param {any} f */
function properties(f) {
  switch (f.ty) {
    case "permission.asked":
    case "question.asked":
      return { id: f.id, sessionID: f.sid }
    case "permission.replied":
    case "question.replied":
    case "question.rejected":
      return { requestID: f.id, sessionID: f.sid }
    case "session.created":
      return { info: { id: f.sid, parentID: f.parentID } }
    case "session.status":
      return { sessionID: f.sid, status: { type: f.s } }
    case "session.error":
      return { sessionID: f.sid, error: { name: f.err } }
    case "session.deleted":
      return { info: { id: f.sid } }
    default:
      return { sessionID: f.sid }
  }
}

/** @param {any} r */
function anchor(r) {
  if (r.hasTool) return r.partStatus === "running" && !r.msgErr && !r.msgCompleted
  if (r.permission === "doom_loop") return (r.doom?.running ?? 0) >= 1 && !r.doom?.msgErr && !r.doom?.msgCompleted
  return false
}

/**
 * A fake OpenCode for one snapshot's request: its session (and ancestor chain) on the server, the anchoring tool
 * part or the doom-loop message as the server returns it, the status map and the TUI store.
 * @param {any} r
 * @param {import("../codeconnect-opencode.js").EpochLog} epoch
 */
function server(r, epoch) {
  const ok = (/** @type {any} */ data) => Promise.resolve({ data, response: { status: 200 } })
  const missing = () => Promise.resolve({ error: { name: "NotFound" }, response: { status: 404 } })
  const chain = [r.sid, ...(r.ancS ?? [])]
  const info = { error: r.msgErr ? { name: r.msgErr } : undefined, time: { created: 1, completed: r.msgCompleted ? 2 : undefined } }
  const input = { command: "same" }
  const client = {
    session: {
      get: (/** @type {any} */ a) => {
        const i = chain.indexOf(a.sessionID)
        if (i < 0 || (i === 0 && !r.sessionExists)) return missing()
        return ok({ id: a.sessionID, ...(chain[i + 1] ? { parentID: chain[i + 1] } : {}) })
      },
      message: () => ok({ info, parts: [{ type: "tool", callID: r.callID, tool: "bash", state: { status: r.partStatus, input } }] }),
      messages: () => {
        const d = r.doom ?? {}
        const last = { info: { role: "assistant", error: d.msgErr ? { name: d.msgErr } : undefined, time: { created: 1, completed: d.msgCompleted ? 2 : undefined } } }
        return ok([{ info: { role: "user", time: { created: 0 } }, parts: [] }, { ...last, parts: (d.matches ?? []).map((/** @type {string} */ st) => ({ type: "tool", tool: "bash", state: { status: st, input } })) }])
      },
    },
  }
  const store = (/** @type {string} */ sid) => (r.inStore && sid === r.sid ? [{ id: r.id }] : [])
  const state = { session: { permission: store, question: store } }
  /** @type {Record<string, any>} */
  const status = {}
  if (r.statusType) status[r.sid] = { type: r.statusType }
  else if (r.busy) status[r.sid] = { type: "busy" }
  const request = {
    id: r.id,
    sessionID: r.sid,
    ...(r.permission ? { permission: r.permission } : {}),
    ...(r.hasTool ? { tool: { messageID: "msg_anchor", callID: r.callID } } : {}),
    ...(r.permission === "doom_loop" ? { metadata: { tool: "bash", input } } : {}),
  }
  return { env: { client, state, epoch, directory: "/Users/ada/project" }, status, request }
}

/**
 * Every judged snapshot of one case, through the rule over the fixture's recorded facts (`facts`) or through the
 * plugin's own judge() against a fake server built from those facts (`judge`).
 * @param {any} c
 * @param {"facts" | "judge"} how
 */
async function replay(c, how) {
  /** @type {Map<string, { live: boolean, dead: string | null }>} */
  const out = new Map()
  let log = new EpochLog()
  for (const f of c.frames) {
    if (f.kind === "loaded") log = new EpochLog()
    if (f.kind === "ev") log.note(f.seq, f.ty, properties(f))
    if (f.kind === "snap")
      for (const r of f.reqs) {
        let v
        if (how === "facts") {
          const chain = [...new Set([...(r.ancS ?? []), ...log.ancestors(r.sid)])]
          v = verdict({ listed: r.listed, inStore: r.inStore, sessionExists: r.sessionExists, busy: r.busy, anchor: anchor(r), epoch: log.ended(r.id, r.sid, chain) })
        } else {
          const s = server(r, log)
          v = await judge(s.env, r.kind, s.request, r.listed, s.status, new Map())
        }
        out.set(f.n + ":" + r.id, v)
      }
  }
  return out
}

/**
 * @param {any[]} cases
 * @param {"facts" | "judge"} how
 */
async function score(cases, how) {
  let judged = 0
  let falseLive = 0
  /** @type {Set<string>} */
  const disagree = new Set()
  for (const c of cases) {
    const got = await replay(c, how)
    for (const j of c.judged) {
      const v = got.get(j.snap + ":" + j.request)
      if (!v) throw new Error(`${c.case}: snapshot ${j.snap} has no verdict for ${j.request}`)
      judged++
      if (v.live && j.oracle === "DEAD") falseLive++
      if (v.live !== (j.oracle === "LIVE")) disagree.add(c.case)
    }
  }
  return { judged, falseLive, disagree: [...disagree].sort() }
}

const known = Object.keys(s6.known_disagreements).sort()

for (const how of /** @type {const} */ (["facts", "judge"])) {
  describe(`verdict (s6-liveness, ${how === "facts" ? "the rule over the recorded facts" : "the plugin's judge() against a fake server"})`, () => {
    for (const c of s6.cases) {
      test(c.case + ": " + c.what, async () => {
        const r = await score([c], how)
        expect(r.falseLive).toBe(0)
        if (how === "judge") expect(await replay(c, "judge")).toEqual(await replay(c, "facts"))
        expect(r.disagree).toEqual(known.includes(c.case) ? [c.case] : [])
      })
    }
    test("no dead request is judged live, and only the designed refusals disagree", async () => {
      const r = await score(s6.cases, how)
      expect(r.judged).toBe(76)
      expect(r.falseLive).toBe(0)
      expect(r.disagree).toEqual(known)
    })
  })
}

describe("verdict rule", () => {
  test("dead reasons name the first failing condition", () => {
    const base = { listed: /** @type {true} */ (true), inStore: true, sessionExists: true, busy: true, anchor: true, epoch: null }
    expect(verdict(base)).toEqual({ live: true, dead: null, listed: true, in_store: true })
    expect(verdict({ ...base, listed: "unknown", inStore: false }).dead).toBe("absent")
    expect(verdict({ ...base, listed: "unknown" }).live).toBe(true)
    expect(verdict({ ...base, sessionExists: false, busy: false }).dead).toBe("session-missing")
    expect(verdict({ ...base, busy: false }).dead).toBe("not-busy")
    expect(verdict({ ...base, anchor: false }).dead).toBe("anchor")
    expect(verdict({ ...base, epoch: "epoch:abort:ancestor" }).dead).toBe("epoch:abort:ancestor")
  })

  test("the log forgets events older than its oldest open request", () => {
    const log = new EpochLog()
    log.note(1, "session.idle", { sessionID: "ses_a" })
    expect(log.events.length).toBe(0)
    log.note(2, "permission.asked", { id: "per_1", sessionID: "ses_a" })
    log.note(3, "session.status", { sessionID: "ses_b", status: { type: "retry" } })
    expect(log.ended("per_1", "ses_a", ["ses_b"])).toBe("epoch:retry:ancestor")
    expect(log.ended("per_0", "ses_a", [])).toBe("asked-before-activation")
    log.note(4, "permission.replied", { requestID: "per_1" })
    expect(log.events.length).toBe(0)
    expect(log.asked.size).toBe(0)
  })

  test("a request whose epoch ends leaves the open set with its reason; repeats are not kept; the list is capped", () => {
    const log = new EpochLog()
    log.note(1, "session.created", { info: { id: "ses_kid", parentID: "ses_root" } })
    log.note(2, "permission.asked", { id: "per_1", sessionID: "ses_kid" })
    log.note(3, "question.asked", { id: "que_1", sessionID: "ses_other" })
    log.note(4, "session.error", { sessionID: "ses_root", error: { name: "MessageAbortedError" } })
    expect([...log.asked.keys()]).toEqual(["que_1"])
    expect(log.ended("per_1", "ses_kid", [])).toBe("epoch:abort:ancestor")
    for (let i = 0; i < 50; i++) log.note(5 + i, "session.idle", { sessionID: "ses_x" })
    expect(log.events.filter((e) => e.sid === "ses_x").length).toBe(1)
    for (let i = 0; i < EPOCH_EVENTS_CAP + 10; i++) log.note(100 + i, "session.idle", { sessionID: "ses_" + i })
    expect(log.events.length).toBeLessThanOrEqual(EPOCH_EVENTS_CAP)
    expect(log.asked.size).toBe(0)
    expect(log.ended("que_1", "ses_other", [])).toBe("asked-before-activation")
    log.note(9000, "session.deleted", { info: { id: "ses_kid" } })
    expect(log.parent.has("ses_kid")).toBe(false)
  })
})
