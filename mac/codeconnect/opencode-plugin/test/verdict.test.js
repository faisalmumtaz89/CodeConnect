// The liveness rule against s6-liveness: each case's events are replayed into a fresh per-activation EpochLog, and at
// every judged snapshot the verdict is computed from the snapshot's server-side facts and that log. No request the
// oracle found dead may be judged live; the only disagreements are the fixture's named, designed refusals.
import { describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { EpochLog, verdict } from "../codeconnect-opencode.js"
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

/** @param {any} c */
function replay(c) {
  /** @type {Map<string, { live: boolean, dead: string | null }>} */
  const out = new Map()
  let log = new EpochLog()
  for (const f of c.frames) {
    if (f.kind === "loaded") log = new EpochLog()
    if (f.kind === "ev") log.note(f.seq, f.ty, properties(f))
    if (f.kind === "snap")
      for (const r of f.reqs) {
        const chain = [...new Set([...(r.ancS ?? []), ...log.ancestors(r.sid)])]
        const v = verdict({ listed: r.listed, inStore: r.inStore, sessionExists: r.sessionExists, busy: r.busy, anchor: anchor(r), epoch: log.ended(r.id, r.sid, chain) })
        out.set(f.n + ":" + r.id, v)
      }
  }
  return out
}

describe("verdict (s6-liveness)", () => {
  let judged = 0
  let falseLive = 0
  /** @type {string[]} */
  const disagree = []
  for (const c of s6.cases) {
    test(c.case + ": " + c.what, () => {
      const got = replay(c)
      for (const j of c.judged) {
        const v = got.get(j.snap + ":" + j.request)
        expect(v).toBeDefined()
        if (!v) continue
        judged++
        if (v.live && j.oracle === "DEAD") falseLive++
        if (v.live !== (j.oracle === "LIVE")) disagree.push(c.case)
      }
    })
  }
  test("no dead request is judged live, and only the designed refusals disagree", () => {
    expect(judged).toBe(76)
    expect(falseLive).toBe(0)
    expect([...new Set(disagree)].sort()).toEqual(Object.keys(s6.known_disagreements).sort())
  })

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
})
