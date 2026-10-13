import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it } from "vitest"

import { ThemeProvider, useTheme } from "./theme-provider.tsx"

function Probe() {
  useTheme()
  return null
}

describe("ThemeProvider", () => {
  // No vitest globals, so testing-library cannot register its own teardown;
  // without this every query finds the previous renders' nodes too.
  afterEach(cleanup)

  beforeEach(() => {
    localStorage.clear()
    document.documentElement.className = ""
  })

  it("starts light and marks the document element with it", () => {
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("light")
  })

  it("a stored theme is the one it opens on", () => {
    localStorage.setItem("theme", "dark")
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("dark")
  })

  it("a stored value that is not a theme falls back to light", () => {
    // The `isTheme` guard, witnessed: without it the stored string would be
    // adopted as a theme and written onto <html> as a class nobody styles.
    localStorage.setItem("theme", "sepia")
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("light")
    localStorage.setItem("theme", "")
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("light")
  })

  it("setTheme persists the choice and swaps the document class", () => {
    function Toggle() {
      const { setTheme } = useTheme()
      return <button onClick={() => setTheme("dark")}>go</button>
    }
    render(
      <ThemeProvider>
        <Toggle />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("light")
    fireEvent.click(screen.getByRole("button"))
    expect(localStorage.getItem("theme")).toBe("dark")
    expect(document.documentElement.className).toBe("dark")
  })

  it("the storage key is the caller's to name", () => {
    localStorage.setItem("appearance", "dark")
    render(
      <ThemeProvider storageKey="appearance">
        <Probe />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("dark")
  })

  it("useTheme outside a provider throws rather than returning nothing", () => {
    expect(() => render(<Probe />)).toThrow(
      "useTheme must be used within a ThemeProvider"
    )
  })
})
