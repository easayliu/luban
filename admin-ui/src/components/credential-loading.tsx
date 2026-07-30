import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'

export function CredentialLoadingState({
  view, selectable = false,
}: {
  view: 'card' | 'list'
  selectable?: boolean
}) {
  return (
    <div role="status" aria-live="polite" aria-label={view === 'card' ? '正在加载账号卡片' : '正在加载账号列表'}>
      <span className="sr-only">加载中</span>
      {view === 'card'
        ? <CardSkeletons selectable={selectable} />
        : <TableSkeletons selectable={selectable} />}
    </div>
  )
}

function CardSkeletons({ selectable }: { selectable: boolean }) {
  return (
    <div className="grid items-stretch gap-3 bg-muted/20 p-2 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))] sm:gap-4 sm:p-4 lg:p-5">
      {Array.from({ length: 4 }, (_, index) => (
        <Card key={index} className="@container/card flex h-full flex-col overflow-hidden rounded-xl border-border/70 shadow-sm">
          <div className="flex items-start gap-3 px-4 py-4 sm:px-5">
            <Skeleton className="grid size-10 shrink-0 place-items-center rounded-full">
              {selectable && <span className="size-3 rounded-sm bg-background/70" />}
            </Skeleton>
            <div className="min-w-0 flex-1 space-y-2 pt-0.5">
              <Skeleton className="h-4 w-4/5" />
              <div className="flex gap-2">
                <Skeleton className="h-5 w-14" />
                <Skeleton className="h-5 w-8" />
                <Skeleton className="h-5 w-16" />
              </div>
            </div>
            <Skeleton className="size-10 shrink-0" />
          </div>

          <div className="flex flex-1 flex-col border-t border-border/70">
            <div className="flex items-center justify-between px-4 pt-4 sm:px-5">
              <Skeleton className="h-4 w-16" />
              <Skeleton className="h-3 w-14" />
            </div>
            <div className="mt-1 grid flex-1 @sm/card:grid-cols-2">
              <QuotaSkeleton className="border-b border-border/70 @sm/card:border-b-0 @sm/card:border-r" />
              <QuotaSkeleton />
            </div>
          </div>

          <div className="mt-auto grid grid-cols-2 border-t border-border/70 bg-muted/20 @lg/card:grid-cols-[repeat(4,minmax(0,1fr))_6rem]">
            {Array.from({ length: 4 }, (_, factIndex) => (
              <div
                key={factIndex}
                className={
                  factIndex === 0
                    ? 'min-h-16 border-b border-r p-3 @lg/card:border-b-0'
                    : factIndex === 1
                      ? 'min-h-16 border-b p-3 @lg/card:border-b-0 @lg/card:border-r'
                      : factIndex === 2
                        ? 'min-h-16 border-r p-3'
                        : 'min-h-16 p-3 @lg/card:border-r'
                }
              >
                <Skeleton className="h-3 w-12" />
                <Skeleton className="mt-2 h-4 w-16" />
              </div>
            ))}
            <div className="col-span-2 flex min-h-14 items-center justify-between border-t px-3 @lg/card:col-span-1 @lg/card:min-h-16 @lg/card:flex-col @lg/card:justify-center @lg/card:gap-1.5 @lg/card:border-t-0">
              <Skeleton className="h-2.5 w-12" />
              <Skeleton className="h-5 w-9 rounded-full" />
            </div>
          </div>
        </Card>
      ))}
    </div>
  )
}

function QuotaSkeleton({ className }: { className?: string }) {
  return (
    <div className={`h-full min-h-24 space-y-3 px-4 pb-4 pt-3 sm:px-5 ${className ?? ''}`}>
      <div className="flex justify-between gap-2">
        <Skeleton className="h-4 w-12" />
        <Skeleton className="h-5 w-10" />
      </div>
      <Skeleton className="h-2 w-full rounded-full" />
      <div className="flex justify-between gap-2">
        <Skeleton className="h-3 w-14" />
        <Skeleton className="h-3 w-16" />
      </div>
    </div>
  )
}

function TableSkeletons({ selectable }: { selectable: boolean }) {
  return (
    <div className="overflow-hidden">
      <div className="hidden xl:block">
        <div className="grid h-10 grid-cols-[2.5rem_minmax(10rem,22fr)_7fr_7fr_9fr_25fr_9fr_12fr_8fr] items-center border-b bg-muted/30">
          <div className="flex justify-center">
            {selectable && <Skeleton className="size-4" />}
          </div>
          <div className="px-3"><Skeleton className="h-3 w-14" /></div>
          <div className="flex justify-center px-2"><Skeleton className="h-3 w-10" /></div>
          <div className="flex justify-center px-2"><Skeleton className="h-3 w-12" /></div>
          <div className="px-2"><Skeleton className="h-3 w-14" /></div>
          <div className="px-3"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2"><Skeleton className="h-3 w-14" /></div>
          <div className="flex justify-end px-3"><Skeleton className="h-3 w-14" /></div>
        </div>
        <div className="divide-y">
          {Array.from({ length: 8 }, (_, index) => (
            <div
              key={index}
              className="grid h-[68px] grid-cols-[2.5rem_minmax(10rem,22fr)_7fr_7fr_9fr_25fr_9fr_12fr_8fr] items-center"
            >
              <div className="flex justify-center">
                {selectable && <Skeleton className="size-4" />}
              </div>
              <div className="min-w-0 px-3">
                <div className="flex min-w-0 items-center gap-3">
                  <Skeleton className="size-10 shrink-0 rounded-full" />
                  <div className="min-w-0 flex-1 space-y-2">
                    <Skeleton className="h-3.5 w-4/5" />
                    <div className="flex items-center gap-2">
                      <Skeleton className="h-2.5 w-12" />
                      <Skeleton className="h-2.5 w-16" />
                    </div>
                  </div>
                </div>
              </div>
              <div className="flex justify-center px-2">
                <Skeleton className="h-5 w-9 rounded-full" />
              </div>
              <div className="flex justify-center px-2">
                <Skeleton className="h-4 w-8" />
              </div>
              <div className="px-2">
                <Skeleton className="h-6 w-16 rounded-md" />
              </div>
              <div className="grid min-w-0 grid-cols-2 gap-4 px-3">
                {Array.from({ length: 2 }, (_, quotaIndex) => (
                  <div key={quotaIndex} className="min-w-0 space-y-2">
                    <div className="flex items-center justify-between gap-2">
                      <Skeleton className="h-3 w-5" />
                      <Skeleton className="h-3 w-8" />
                    </div>
                    <Skeleton className="h-1.5 w-full rounded-full" />
                  </div>
                ))}
              </div>
              <div className="flex min-w-0 items-center gap-2 px-2">
                <Skeleton className="size-4 shrink-0" />
                <div className="min-w-0 flex-1 space-y-1.5">
                  <Skeleton className="h-3 w-10" />
                  <Skeleton className="h-2.5 w-8" />
                </div>
              </div>
              <div className="flex min-w-0 items-center gap-1.5 px-2">
                <Skeleton className="size-4 shrink-0" />
                <Skeleton className="h-3 w-14" />
              </div>
              <div className="flex justify-end px-3">
                <Skeleton className="h-4 w-14" />
              </div>
            </div>
          ))}
        </div>
      </div>

      <div className="divide-y xl:hidden">
        {Array.from({ length: 4 }, (_, index) => (
          <div key={index} className="px-4 py-4 sm:px-5">
            <div className="flex items-start gap-3">
              {selectable && <Skeleton className="mt-3 size-4 shrink-0" />}
              <Skeleton className="size-10 shrink-0 rounded-full" />
              <div className="min-w-0 flex-1 space-y-2">
                <div className="flex items-center gap-2">
                  <Skeleton className="h-4 w-3/5" />
                  <Skeleton className="h-3 w-8" />
                </div>
                <div className="flex gap-3">
                  <Skeleton className="h-3 w-14" />
                  <Skeleton className="h-3 w-20" />
                </div>
              </div>
              <div className="flex shrink-0 flex-col items-end gap-1.5">
                <Skeleton className="h-2.5 w-7" />
                <Skeleton className="h-5 w-9 rounded-full" />
              </div>
            </div>

            <div className="mt-4 grid grid-cols-2 gap-4 border-t border-border/70 pt-4">
              {Array.from({ length: 2 }, (_, quotaIndex) => (
                <div key={quotaIndex} className="min-w-0 space-y-2">
                  <div className="flex items-center justify-between gap-2">
                    <Skeleton className="h-3 w-5" />
                    <Skeleton className="h-3 w-8" />
                  </div>
                  <Skeleton className="h-1.5 w-full rounded-full" />
                </div>
              ))}
            </div>

            <div className="mt-4 grid grid-cols-2 overflow-hidden rounded-lg border border-border/70 bg-muted/10 sm:grid-cols-3">
              {Array.from({ length: 6 }, (_, factIndex) => (
                <div
                  key={factIndex}
                  className={
                    factIndex === 0
                      ? 'min-w-0 border-b border-r px-3 py-3 sm:border-b'
                      : factIndex === 1
                        ? 'min-w-0 border-b px-3 py-3 sm:border-r'
                        : factIndex === 2
                          ? 'min-w-0 border-b border-r px-3 py-3 sm:border-b sm:border-r-0'
                          : factIndex === 3
                            ? 'min-w-0 border-b px-3 py-3 sm:border-b-0 sm:border-r'
                            : factIndex === 4
                              ? 'min-w-0 border-r px-3 py-3'
                              : 'min-w-0 px-3 py-3'
                  }
                >
                  <Skeleton className="h-2.5 w-12" />
                  <Skeleton className="mt-2 h-3 w-14" />
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
