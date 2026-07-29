export function AppFooter() {
  return (
    <>
      <div className="app-footer-spacer shrink-0" aria-hidden />
      <footer className="app-footer-bar fixed inset-x-0 bottom-0 z-30 border-t border-border/80 bg-background/95 backdrop-blur-xl">
        <div className="mx-auto flex w-full max-w-7xl items-center justify-between gap-4 px-4 py-3.5 text-2xs text-muted-foreground lg:px-6">
          <span className="flex min-w-0 items-center gap-2">
            <span className="font-semibold text-foreground/80">Luban</span>
            <span className="hidden font-mono uppercase tracking-wider sm:inline">Claude Code Gateway</span>
          </span>
          <span className="font-mono tnum">v{__APP_VERSION__}</span>
        </div>
      </footer>
    </>
  )
}
