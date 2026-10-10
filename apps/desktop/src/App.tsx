import { Button } from "@workspace/ui/components/button"
import { useTheme } from "@/components/theme-provider.tsx"

export function App() {
  const { theme, setTheme } = useTheme()

  return (
    <div className="flex min-h-svh p-6">
      <div className="flex max-w-md min-w-0 flex-col gap-4 text-sm leading-loose">
        <div>
          <h1 className="font-medium">Momotaro</h1>
          <p>Product UI — the future Tauri shell around the engine.</p>
          <Button
            className="mt-2"
            onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          >
            Toggle theme
          </Button>
        </div>
      </div>
    </div>
  )
}