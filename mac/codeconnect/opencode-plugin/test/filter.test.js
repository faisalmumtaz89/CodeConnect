// The forwarding filter against the s8 captures: every bus frame the plugin observed, in order, through the filter
// gives exactly the forwarded file. Frames flagged `undelivered` (the link was down) still pass through the filter
// so its state stays as it was live, but are not compared; `ccd.*` rows are the daemon's own records, and a
// `ccd.link.opened` with `new_process` starts a fresh activation.
import { describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { Filter, SUBSCRIBED } from "../codeconnect-opencode.js"
import { fixture } from "./helpers.js"

const SETS = ["timeline", "resume-after-revert", "lifecycle", "dispose-childerr", "resync"]
const lines = (/** @type {string} */ name) => readFileSync(fixture(name), "utf8").split("\n").filter(Boolean)

describe("filter", () => {
  for (const set of SETS) {
    test(`s8-${set}: bus -> forwarded, byte for byte`, () => {
      const bus = lines(`s8-${set}-bus-1.18.34.jsonl`).map((l) => JSON.parse(l))
      const want = lines(`s8-${set}-forwarded-1.18.34.jsonl`).filter((l) => !JSON.parse(l).type.startsWith("ccd."))
      let f = new Filter()
      const got = []
      for (const e of bus) {
        if (e.type.startsWith("ccd.")) {
          if (e.type === "ccd.link.opened" && e.properties.new_process) f = new Filter()
          continue
        }
        const out = f.apply(e.type, e.properties)
        if (out !== null && !e.undelivered) got.push(JSON.stringify({ type: e.type, properties: out }))
      }
      expect(got.length).toBe(want.length)
      for (let i = 0; i < want.length; i++) expect(got[i]).toBe(want[i])
    })
  }

  test("subscribes to no delta, session.next or v2 event", () => {
    for (const t of SUBSCRIBED) {
      expect(t).not.toBe("message.part.delta")
      expect(t.startsWith("session.next.")).toBe(false)
      expect(t.includes(".v2.")).toBe(false)
    }
    expect(new Filter().apply("message.part.delta", { delta: "x" })).toBeNull()
    expect(new Filter().apply("session.diff", { diff: [] })).toBeNull()
  })

  test("maps lose each tool, text and assistant entry at its terminal", () => {
    const bus = lines("s8-timeline-bus-1.18.34.jsonl").map((l) => JSON.parse(l))
    const f = new Filter()
    for (const e of bus) if (!e.type.startsWith("ccd.")) f.apply(e.type, e.properties)
    expect(f.toolSent.size).toBe(0)
    expect(f.started.size).toBe(0)
    for (const sig of f.messageSent.values()) expect(JSON.parse(sig)[0]).toBe("user")
  })
})
