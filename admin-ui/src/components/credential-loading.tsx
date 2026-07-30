import {
  Card,
  CardAction,
  CardDescription,
  CardFooter,
  CardHeader,
  CardPanel,
  CardTitle,
} from '@/components/ui/card'
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
    <div className="grid items-stretch gap-4 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))]">
      {Array.from({ length: 4 }, (_, index) => (
        <Card key={index} className="@container/card h-full overflow-hidden">
          <CardHeader>
            <CardTitle>
              <div className="flex items-center gap-3">
                {selectable && <Skeleton className="size-4 shrink-0" />}
                <Skeleton className="size-8 shrink-0 rounded-full" />
                <div className="min-w-0 flex-1 space-y-2">
                  <Skeleton className="h-5 w-3/5" />
                  <CardDescription className="flex gap-2">
                    <Skeleton className="h-4 w-10" />
                    <Skeleton className="h-4 w-24" />
                  </CardDescription>
                </div>
              </div>
            </CardTitle>
            <CardAction><Skeleton className="size-8" /></CardAction>
          </CardHeader>

          <CardPanel className="space-y-5">
            <div className="flex items-center gap-2">
              <Skeleton className="h-5 w-16" />
              <Skeleton className="h-5 w-9" />
              <Skeleton className="ml-auto h-4 w-24" />
            </div>
            <div className="flex items-center justify-between gap-3">
              <Skeleton className="h-4 w-16" />
            </div>
            <div className="grid gap-5 @sm/card:grid-cols-2">
              <QuotaSkeleton />
              <QuotaSkeleton />
            </div>
          </CardPanel>

          <CardFooter className="mt-auto justify-between gap-3 border-t bg-muted/32">
            <Skeleton className="h-8 w-28" />
            <div className="flex items-center gap-2">
              <Skeleton className="h-5 w-16" />
              <Skeleton className="h-5 w-9 rounded-full" />
            </div>
          </CardFooter>
        </Card>
      ))}
    </div>
  )
}

function QuotaSkeleton() {
  return (
    <div className="space-y-2">
      <div className="flex justify-between gap-2">
        <Skeleton className="h-4 w-12" />
        <Skeleton className="h-4 w-10" />
      </div>
      <Skeleton className="h-2 w-full" />
      <div className="flex justify-between gap-2">
        <Skeleton className="h-3 w-14" />
        <Skeleton className="h-3 w-16" />
      </div>
    </div>
  )
}

function TableSkeletons({ selectable }: { selectable: boolean }) {
  const desktopColumns = selectable
    ? 'grid-cols-[2.5rem_minmax(10rem,22fr)_8fr_7fr_9fr_25fr_9fr_12fr_8fr]'
    : 'grid-cols-[0.75rem_minmax(10rem,22fr)_8fr_7fr_9fr_25fr_9fr_12fr_8fr]'

  return (
    <div className="overflow-hidden">
      <div className="hidden xl:block">
        <div className={`grid h-10 items-center border-b bg-muted/30 ${desktopColumns}`}>
          <div className="flex justify-center">
            {selectable && <Skeleton className="size-4" />}
          </div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-12" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-10" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
          <div className="px-2.5"><Skeleton className="h-3 w-14" /></div>
        </div>
        <div className="divide-y">
          {Array.from({ length: 8 }, (_, index) => (
            <div
              key={index}
              className={`grid h-[68px] items-center ${desktopColumns}`}
            >
              <div className="flex justify-center">
                {selectable && <Skeleton className="size-4" />}
              </div>
              <div className="min-w-0 space-y-2 px-2.5">
                <Skeleton className="h-3.5 w-4/5" />
                <div className="flex items-center gap-2">
                  <Skeleton className="h-2.5 w-12" />
                  <Skeleton className="h-2.5 w-16" />
                </div>
              </div>
              <div className="px-2.5">
                <Skeleton className="h-5 w-9 rounded-full" />
              </div>
              <div className="px-2.5">
                <Skeleton className="h-4 w-8" />
              </div>
              <div className="px-2.5">
                <Skeleton className="h-6 w-16 rounded-md" />
              </div>
              <div className="grid min-w-0 grid-cols-2 gap-4 px-2.5">
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
              <div className="min-w-0 space-y-1.5 px-2.5">
                <Skeleton className="h-3 w-10" />
                <Skeleton className="h-2.5 w-8" />
              </div>
              <div className="min-w-0 px-2.5">
                <Skeleton className="h-3 w-14" />
              </div>
              <div className="px-2.5">
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
              <div className="min-w-0 flex-1 space-y-2">
                <div className="flex items-center gap-2">
                  <Skeleton className="h-4 w-3/5" />
                  <Skeleton className="h-3 w-8" />
                </div>
                <Skeleton className="h-3 w-20" />
              </div>
              <Skeleton className="h-5 w-9 shrink-0 rounded-full" />
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

            <div className="mt-4 grid grid-cols-2 gap-4 border-t border-border/70 pt-4 sm:grid-cols-3">
              {Array.from({ length: 3 }, (_, factIndex) => (
                <div key={factIndex} className="min-w-0">
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
