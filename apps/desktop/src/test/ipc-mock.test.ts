import { invoke } from "@tauri-apps/api/core"
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks"
import { afterEach, describe, expect, it } from "vitest"

// The shell boundary's mock layer. This app calls **no** Tauri command yet —
// there is no `src-tauri` in this tree — so what is pinned here is the rig
// itself, in Tauri's own words: `@tauri-apps/api/mocks` is the first of the
// three tiers the docs describe (mock IPC, mock runtime, WebDriver), and it
// is the tier a unit test uses. The day the first `invoke` lands in this app,
// its test routes the command through `mockIPC` here — rather than anyone
// hand-rolling `window.__TAURI_INTERNALS__`, which would be a second mock
// with its own drift.
//
// The command name is deliberately not a product name: inventing one here
// would be inventing an IPC contract the engine does not have.
describe("the shell boundary's mock layer", () => {
  afterEach(clearMocks)

  it("routes an invoke through the mock and answers with its value", async () => {
    mockIPC((cmd, args) =>
      cmd === "echo"
        ? { said: args }
        : Promise.reject(new Error("no such command"))
    )
    await expect(invoke("echo", { word: "momotaro" })).resolves.toEqual({
      said: { word: "momotaro" },
    })
  })

  it("clearing the mocks puts the boundary back", async () => {
    mockIPC(() => "from the mock")
    clearMocks()
    await expect(invoke("echo")).rejects.toThrow()
  })

  it("a command the mock does not answer is a rejection, not silence", async () => {
    mockIPC((cmd) =>
      cmd === "echo"
        ? "from the mock"
        : Promise.reject(new Error("no such command"))
    )
    await expect(invoke("something-else")).rejects.toThrow("no such command")
  })
})
