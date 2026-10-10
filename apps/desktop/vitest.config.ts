import { defineConfig } from "vitest/config"
import path from "path"
import react from "@vitejs/plugin-react"

// The test rig's own config: `vite.config.ts` is the build's, and pulling the
// Tailwind plugin into jsdom buys nothing the tests can see. The one thing
// copied is the `@` alias, because the components under test use it.
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
})
