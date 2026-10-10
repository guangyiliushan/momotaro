import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it } from "vitest"

import { App } from "./App.tsx"
import { ThemeProvider } from "@/components/theme-provider.tsx"

describe("App", () => {
  afterEach(cleanup)
  beforeEach(() => {
    localStorage.clear()
    document.documentElement.className = ""
  })

  it("renders the product shell's opening screen", () => {
    render(
      <ThemeProvider>
        <App />
      </ThemeProvider>
    )
    expect(screen.getByText("Momotaro")).toBeTruthy()
    expect(
      screen.getByText("Product UI — the future Tauri shell around the engine.")
    ).toBeTruthy()
  })

  it("the one button flips the theme the shell is in", () => {
    render(
      <ThemeProvider>
        <App />
      </ThemeProvider>
    )
    expect(document.documentElement.className).toBe("light")

    fireEvent.click(screen.getByRole("button", { name: "Toggle theme" }))
    expect(document.documentElement.className).toBe("dark")
    expect(localStorage.getItem("theme")).toBe("dark")

    fireEvent.click(screen.getByRole("button", { name: "Toggle theme" }))
    expect(document.documentElement.className).toBe("light")
    expect(localStorage.getItem("theme")).toBe("light")
  })
})
