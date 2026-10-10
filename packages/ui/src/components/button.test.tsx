import { cleanup, render, screen } from "@testing-library/react"
import { afterEach, describe, expect, it } from "vitest"

import { buttonVariants, Button } from "./button"

describe("Button", () => {
  // No vitest globals, so testing-library cannot register its own teardown:
  // the document is cleaned here, or the second render finds the first
  // render's button as well and every query becomes "found two".
  afterEach(cleanup)

  it("is the element labelled by its children", () => {
    render(<Button>Toggle theme</Button>)
    const button = screen.getByRole("button", { name: "Toggle theme" })
    expect(button.getAttribute("data-slot")).toBe("button")
  })

  it("carries the chosen variant and size into the class list", () => {
    render(
      <Button variant="destructive" size="lg">
        drop
      </Button>
    )
    const classes = screen.getByRole("button").className
    // The two are cva's own outputs for these choices — assert the literal
    // tailwind classes rather than a rendered snapshot, so a variant that
    // silently falls back to `default` is visible as a missing class.
    expect(classes).toContain("bg-destructive/10")
    expect(classes).toContain("h-9")
  })

  it("a caller's className overrides the variant's own", () => {
    render(
      <Button variant="default" className="bg-primary/10">
        x
      </Button>
    )
    const classes = screen.getByRole("button").className
    expect(classes).toContain("bg-primary/10")
    expect(classes).not.toContain("bg-primary ")
  })

  it("disabled reaches the element, not only its styling", () => {
    render(<Button disabled>wait</Button>)
    expect(screen.getByRole("button").hasAttribute("disabled")).toBe(true)
  })

  it("clicking it runs the handler the caller passed", () => {
    let clicks = 0
    render(<Button onClick={() => (clicks += 1)}>go</Button>)
    screen.getByRole("button").click()
    expect(clicks).toBe(1)
  })

  it("buttonVariants is the same recipe the component applies", () => {
    expect(buttonVariants({ variant: "ghost" })).toContain("hover:bg-muted")
    expect(buttonVariants({ variant: "ghost" })).not.toContain("bg-primary")
  })
})
