import type * as React from "react";
import { cn } from "@/lib/utils";

export function Frame({
  className,
  ...props
}: React.ComponentProps<"div">): React.ReactElement {
  return (
    <div
      className={cn(
        "relative flex flex-col rounded-2xl bg-muted/72 p-1",
        "*:[[data-slot=frame-panel]+[data-slot=frame-panel]]:mt-1",
        // 窄屏铺到屏幕边，并收掉灰托盘。
        // 桌面上「灰托盘 4px + 面板边框 1px」是廉价的层级记号；375px 上它和 page-frame 的
        // 16px 加起来，让一行设置的文字从第 37px 才开始，两侧共吃掉 74px（近五分之一屏宽）。
        // `-mx-4` 正好抵掉 page-frame 那 1rem；刘海屏上 padding 是 `max(1rem, safe-area)`，
        // 抵掉 16px 之后卡片停在安全区边界而不是钻到刘海下面，正是想要的。
        "max-sm:-mx-4 max-sm:rounded-none max-sm:bg-transparent max-sm:p-0",
        className,
      )}
      data-slot="frame"
      {...props}
    />
  );
}

export function FramePanel({
  className,
  ...props
}: React.ComponentProps<"div">): React.ReactElement {
  return (
    <div
      className={cn(
        "relative rounded-xl border bg-background bg-clip-padding p-4 shadow-xs/5 sm:p-5 before:pointer-events-none before:absolute before:inset-0 before:rounded-[calc(var(--radius-xl)-1px)] before:shadow-[0_1px_var(--bevel)] dark:before:shadow-[0_-1px_var(--bevel)]",
        // 铺到边之后左右边框没有意义（边框外面已经没有东西了），只留上下两条分隔线。
        "max-sm:rounded-none max-sm:border-x-0 max-sm:shadow-none max-sm:before:hidden",
        className,
      )}
      data-slot="frame-panel"
      {...props}
    />
  );
}

export function FrameHeader({
  className,
  ...props
}: React.ComponentProps<"header">): React.ReactElement {
  return (
    <header
      className={cn("flex flex-col px-4 py-4 sm:px-5", className)}
      data-slot="frame-panel-header"
      {...props}
    />
  );
}

export function FrameTitle({
  className,
  ...props
}: React.ComponentProps<"div">): React.ReactElement {
  return (
    <div
      className={cn("font-semibold text-sm", className)}
      data-slot="frame-panel-title"
      {...props}
    />
  );
}

export function FrameDescription({
  className,
  ...props
}: React.ComponentProps<"div">): React.ReactElement {
  return (
    <div
      className={cn("text-muted-foreground text-sm", className)}
      data-slot="frame-panel-description"
      {...props}
    />
  );
}

export function FrameFooter({
  className,
  ...props
}: React.ComponentProps<"footer">): React.ReactElement {
  return (
    <footer
      className={cn("px-4 py-4 sm:px-5", className)}
      data-slot="frame-panel-footer"
      {...props}
    />
  );
}
