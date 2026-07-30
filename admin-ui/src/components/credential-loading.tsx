import { Card, CardContent, CardHeader } from '@/components/ui/card'
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
    <div className="grid items-start gap-3 bg-muted/25 p-2 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))] sm:gap-4 sm:p-4">
      {Array.from({ length: 4 }, (_, index) => (
        <Card key={index} className="overflow-hidden rounded-xl border-border/80 shadow-none">
          <CardHeader className="space-y-3 p-4">
            <div className="flex items-start gap-3">
              {selectable && <Skeleton className="mt-2 size-4 shrink-0" />}
              <Skeleton className="size-9 shrink-0 rounded-full" />
              <div className="min-w-0 flex-1 space-y-2 pt-0.5">
                <Skeleton className="h-4 w-4/5" />
                <Skeleton className="h-3 w-12" />
              </div>
              <Skeleton className="mt-1 h-5 w-9 shrink-0 rounded-full" />
              <Skeleton className="size-8 shrink-0" />
            </div>
            <div className="flex gap-1.5">
              <Skeleton className="h-5 w-14" />
              <Skeleton className="h-5 w-16" />
              <Skeleton className="h-5 w-8" />
            </div>
          </CardHeader>
          <CardContent className="space-y-3 p-4 pt-0">
            <div className="grid grid-cols-2 gap-2">
              <QuotaSkeleton />
              <QuotaSkeleton />
            </div>
            <div className="grid grid-cols-3 gap-3 border-y py-3">
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
            </div>
            <div className="flex items-center justify-between border-t pt-3">
              <div className="space-y-1.5">
                <Skeleton className="h-3 w-14" />
                <Skeleton className="h-4 w-20" />
              </div>
              <Skeleton className="h-5 w-9 rounded-full" />
            </div>
          </CardContent>
        </Card>
      ))}
    </div>
  )
}

function QuotaSkeleton() {
  return (
    <div className="space-y-2 rounded-md border bg-muted/10 p-2.5">
      <div className="flex justify-between gap-2">
        <Skeleton className="h-3 w-5" />
        <Skeleton className="h-3 w-8" />
      </div>
      <Skeleton className="h-1.5 w-full rounded-full" />
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
      <div className="hidden lg:block">
        <div className="grid h-10 grid-cols-[1rem_minmax(10rem,30fr)_12fr_28fr_12fr_14fr_2rem] items-center gap-3 border-b px-3">
          {selectable ? <Skeleton className="size-4" /> : <span />}
          {Array.from({ length: 5 }, (_, index) => (
            <Skeleton key={index} className="h-3 w-3/5" />
          ))}
          <span />
        </div>
        <div className="divide-y">
          {Array.from({ length: 8 }, (_, index) => (
            <div
              key={index}
              className="grid h-12 grid-cols-[1rem_minmax(10rem,30fr)_12fr_28fr_12fr_14fr_2rem] items-center gap-3 px-3"
            >
              {selectable ? <Skeleton className="size-4" /> : <span />}
              <div className="flex min-w-0 items-center gap-3">
                <Skeleton className="size-8 shrink-0 rounded-full" />
                <div className="min-w-0 flex-1 space-y-1.5">
                  <Skeleton className="h-3 w-3/4" />
                  <Skeleton className="h-2.5 w-10" />
                </div>
              </div>
              <Skeleton className="h-5 w-9 rounded-full" />
              <div className="grid grid-cols-2 gap-3">
                <Skeleton className="h-6 w-full" />
                <Skeleton className="h-6 w-full" />
              </div>
              <Skeleton className="h-4 w-10" />
              <Skeleton className="h-4 w-16" />
              <Skeleton className="size-7" />
            </div>
          ))}
        </div>
      </div>

      <div className="divide-y lg:hidden">
        {Array.from({ length: 4 }, (_, index) => (
          <div key={index} className="space-y-3 p-3.5">
            <div className="flex items-start gap-3">
              {selectable && <Skeleton className="mt-2 size-4 shrink-0" />}
              <Skeleton className="size-9 shrink-0 rounded-full" />
              <div className="min-w-0 flex-1 space-y-2">
                <Skeleton className="h-4 w-4/5" />
                <div className="flex gap-1.5">
                  <Skeleton className="h-5 w-14" />
                  <Skeleton className="h-5 w-12" />
                </div>
              </div>
              <Skeleton className="size-8 shrink-0" />
            </div>
            <div className="grid grid-cols-2 gap-4">
              <Skeleton className="h-7 w-full" />
              <Skeleton className="h-7 w-full" />
            </div>
            <div className="flex justify-between gap-3">
              <Skeleton className="h-3 w-16" />
              <Skeleton className="h-3 w-14" />
              <Skeleton className="h-3 w-16" />
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
