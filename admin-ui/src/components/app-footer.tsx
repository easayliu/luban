export function AppFooter() {
  return (
    <footer className="app-footer-bar mt-auto border-t border-border/70 bg-card">
      <div className="page-frame flex items-center justify-between gap-4 py-4 text-xs text-muted-foreground">
        <span className="flex min-w-0 items-center gap-2">
          <span className="font-semibold text-foreground/80">Luban</span>
          <span className="hidden sm:inline">Claude Code Gateway</span>
        </span>
        <span className="font-mono tnum">v{__APP_VERSION__}</span>
      </div>
    </footer>
  )
}
