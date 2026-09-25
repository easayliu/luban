"use client";

import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { mergeProps } from "@base-ui/react/merge-props";
import { useRender } from "@base-ui/react/use-render";
import { XIcon } from "lucide-react";
import type React from "react";
import { cn } from "@/lib/utils";
import { useI18n } from "@/lib/i18n";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";

export const DialogCreateHandle: typeof DialogPrimitive.createHandle =
  DialogPrimitive.createHandle;

export const Dialog: typeof DialogPrimitive.Root = DialogPrimitive.Root;

export const DialogPortal: typeof DialogPrimitive.Portal =
  DialogPrimitive.Portal;

export function DialogTrigger(
  props: DialogPrimitive.Trigger.Props,
): React.ReactElement {
  return <DialogPrimitive.Trigger data-slot="dialog-trigger" {...props} />;
}

export function DialogClose(
  props: DialogPrimitive.Close.Props,
): React.ReactElement {
  return <DialogPrimitive.Close data-slot="dialog-close" {...props} />;
}

export function DialogBackdrop({
  className,
  ...props
}: DialogPrimitive.Backdrop.Props): React.ReactElement {
  return (
    <DialogPrimitive.Backdrop
      className={cn(
        "fixed inset-0 z-50 bg-black/40 transition-all duration-200 data-ending-style:opacity-0 data-starting-style:opacity-0",
        className,
      )}
      data-slot="dialog-backdrop"
      {...props}
    />
  );
}

export function DialogViewport({
  className,
  ...props
}: DialogPrimitive.Viewport.Props): React.ReactElement {
  return (
    <DialogPrimitive.Viewport
      className={cn(
        "fixed inset-0 z-50 grid grid-rows-[1fr_auto_3fr] justify-items-center p-4",
        className,
      )}
      data-slot="dialog-viewport"
      {...props}
    />
  );
}

/**
 * 弹窗宽度的固定档位。以前每个调用处自己写 `max-w-2xl` / `sm:max-w-3xl` / `max-w-6xl`，
 * 两种写法混用，还有四处写的就是底座默认值（纯冗余）。档位收进这里，调用处只挑名字。
 */
const DIALOG_SIZES = {
  sm: "max-w-lg",
  md: "max-w-2xl",
  lg: "max-w-3xl",
  xl: "max-w-4xl",
  full: "max-w-6xl",
} as const;

export type DialogSize = keyof typeof DIALOG_SIZES;

export function DialogPopup({
  className,
  children,
  size = "sm",
  showCloseButton = true,
  bottomStickOnMobile = true,
  closeProps,
  portalProps,
  ...props
}: DialogPrimitive.Popup.Props & {
  size?: DialogSize;
  showCloseButton?: boolean;
  bottomStickOnMobile?: boolean;
  closeProps?: DialogPrimitive.Close.Props;
  portalProps?: DialogPrimitive.Portal.Props;
}): React.ReactElement {
  const { t } = useI18n();

  return (
    <DialogPortal {...portalProps}>
      <DialogBackdrop />
      <DialogViewport
        className={cn(
          bottomStickOnMobile &&
            "max-sm:grid-rows-[1fr_auto] max-sm:p-0 max-sm:pt-[max(3rem,env(safe-area-inset-top))]",
        )}
      >
        {/* 手机上贴底：左右与底边铺满，顶部留 2xl 圆角——与账号操作面板同一副外形，
            整站的底部弹层（确认框、对话框、操作面板）在手机上长得一样。 */}
        <DialogPrimitive.Popup
          className={cn(
            "relative row-start-2 flex max-h-full min-h-0 w-full min-w-0 max-w-lg origin-center flex-col rounded-2xl border bg-popover not-dark:bg-clip-padding text-popover-foreground opacity-[calc(1-var(--nested-dialogs))] shadow-lg outline-none transition-[scale,opacity,translate] duration-200 ease-in-out will-change-transform before:pointer-events-none before:absolute before:inset-0 before:rounded-[calc(var(--radius-2xl)-1px)] before:shadow-[0_1px_var(--bevel)] data-ending-style:opacity-0 data-starting-style:opacity-0 sm:scale-[calc(1-0.1*var(--nested-dialogs))] sm:data-ending-style:scale-98 sm:data-starting-style:scale-98 dark:before:shadow-[0_-1px_var(--bevel)]",
            DIALOG_SIZES[size],
            bottomStickOnMobile &&
              "max-sm:max-w-none max-sm:origin-bottom max-sm:rounded-none max-sm:rounded-t-2xl max-sm:border-x-0 max-sm:border-t max-sm:border-b-0 max-sm:pb-[env(safe-area-inset-bottom)] max-sm:data-ending-style:translate-y-4 max-sm:data-starting-style:translate-y-4 max-sm:before:hidden max-sm:before:rounded-none",
            className,
          )}
          data-slot="dialog-popup"
          {...props}
        >
          {children}
          {showCloseButton && (
            <DialogPrimitive.Close
              aria-label={t("关闭", "Close")}
              className="absolute end-2 top-2"
              render={<Button size="icon" variant="ghost" />}
              {...closeProps}
            >
              <XIcon />
            </DialogPrimitive.Close>
          )}
        </DialogPrimitive.Popup>
      </DialogViewport>
    </DialogPortal>
  );
}

/**
 * `panel` 变体：带下分隔线与浅灰底，给的是「头部本身有内容」的那类弹窗——趋势、明细里头部
 * 放着头像、标题、副标题和时间范围切换，需要和下面的图表分开。
 *
 * 以前这三个弹窗各自在 className 里写 `border-b bg-muted/32 p-4 sm:p-5`，连内边距都跟别处
 * 不一样，等于凭空多出第二套弹窗密度。现在它是底座提供的一个选择，槽宽仍是统一的 24px。
 */
export function DialogHeader({
  className,
  variant = "default",
  render,
  ...props
}: useRender.ComponentProps<"div"> & {
  variant?: "default" | "panel";
}): React.ReactElement {
  const defaultProps = {
    className: cn(
      // 槽宽分两档：手机 16px、≥640px 24px。手机上弹窗是整屏的底部抽屉，24px 太肥。
      "flex flex-col gap-2 p-4 sm:p-6",
      // 紧挨着 panel 时收一收下边距；panel 变体靠分隔线分隔，不收。
      variant === "default" &&
        "in-[[data-slot=dialog-popup]:has([data-slot=dialog-panel])]:pb-3",
      "max-sm:pb-4",
      variant === "panel" && "border-b bg-muted/32",
      className,
    ),
    "data-slot": "dialog-header",
    "data-variant": variant,
  };

  return useRender({
    defaultTagName: "div",
    props: mergeProps<"div">(defaultProps, props),
    render,
  });
}

export function DialogFooter({
  className,
  variant = "default",
  render,
  ...props
}: useRender.ComponentProps<"div"> & {
  variant?: "default" | "bare";
}): React.ReactElement {
  const defaultProps = {
    className: cn(
      "flex flex-col-reverse gap-2 px-4 sm:flex-row sm:justify-end sm:px-6 sm:rounded-b-[calc(var(--radius-2xl)-1px)]",
      variant === "default" && "border-t bg-muted/72 py-4",
      variant === "bare" &&
        "in-[[data-slot=dialog-popup]:has([data-slot=dialog-panel])]:pt-3 pt-4 pb-6",
      className,
    ),
    "data-slot": "dialog-footer",
  };

  return useRender({
    defaultTagName: "div",
    props: mergeProps<"div">(defaultProps, props),
    render,
  });
}

export function DialogTitle({
  className,
  ...props
}: DialogPrimitive.Title.Props): React.ReactElement {
  return (
    <DialogPrimitive.Title
      className={cn(
        // 明细/趋势那几个弹窗开时会把焦点移到标题（initialFocus={titleRef} + tabIndex=-1），
        // 目的是把读屏光标带到弹窗开头，标题本身不可交互、也不在 Tab 序列里。
        // 不关掉轮廓的话，浏览器会在这里画自己那圈默认蓝——整个弹窗里唯一一处非主题色。
        "font-heading font-semibold text-lg leading-none outline-none sm:text-xl",
        className,
      )}
      data-slot="dialog-title"
      {...props}
    />
  );
}

export function DialogDescription({
  className,
  ...props
}: DialogPrimitive.Description.Props): React.ReactElement {
  return (
    <DialogPrimitive.Description
      className={cn("text-muted-foreground text-sm", className)}
      data-slot="dialog-description"
      {...props}
    />
  );
}

export function DialogPanel({
  className,
  scrollFade = true,
  render,
  ...props
}: useRender.ComponentProps<"div"> & {
  scrollFade?: boolean;
}): React.ReactElement {
  const defaultProps = {
    className: cn(
      "p-4 sm:p-6 in-[[data-slot=dialog-popup]:has([data-slot=dialog-header]:not([data-variant=panel]))]:pt-1 in-[[data-slot=dialog-popup]:has([data-slot=dialog-footer]:not(.border-t))]:pb-1",
      className,
    ),
    "data-slot": "dialog-panel",
  };

  return (
    <ScrollArea scrollFade={scrollFade}>
      {useRender({
        defaultTagName: "div",
        props: mergeProps<"div">(defaultProps, props),
        render,
      })}
    </ScrollArea>
  );
}

export {
  DialogPrimitive,
  DialogBackdrop as DialogOverlay,
  DialogPopup as DialogContent,
};
