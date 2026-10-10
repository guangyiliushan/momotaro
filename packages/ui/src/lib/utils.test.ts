import { describe, expect, it } from "vitest"

import { cn } from "./utils"

// A value, not a constant `&&`: a falsy branch has to be one the linter
// agrees is undecided at the point of the call.
const notACondition = undefined

// `utils.ts` is a one-line re-export, so what is pinned is the contract the
// components above it rely on: conditional joining (clsx) and tailwind
// conflict resolution (tailwind-merge), in one call. A `cn` that only joined
// would still render, and the variant overrides in `button.tsx` would quietly
// stop overriding.
describe("cn", () => {
  it("joins the truthy branches and drops the rest", () => {
    expect(cn("px-2", notACondition, "py-1", null, undefined)).toBe("px-2 py-1")
  })

  it("resolves a tailwind conflict by keeping the later class", () => {
    expect(cn("px-2 py-1", "px-4")).toBe("py-1 px-4")
  })

  it("lets a caller's class override the component's default for the same property", () => {
    expect(cn("bg-primary text-primary-foreground", "bg-primary/10")).toBe(
      "text-primary-foreground bg-primary/10"
    )
  })

  it("takes the object form like clsx", () => {
    expect(cn({ "text-white": true, hidden: false })).toBe("text-white")
  })
})
