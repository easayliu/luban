export function AppFooter() {
  return (
    <footer className="app-footer-bar mt-auto border-t border-border/70 bg-background/70">
      <div className="page-frame flex items-center justify-between gap-4 py-3.5 text-2xs text-muted-foreground">
        <span className="flex min-w-0 items-center gap-2">
          <span className="font-semibold text-foreground/80">Luban</span>
          <span className="hidden font-mono uppercase tracking-wider sm:inline">Claude Code Gateway</span>
        </span>
        <span className="font-mono tnum">v{__APP_VERSION__}</span>
      </div>
    </footer>
  )
}
