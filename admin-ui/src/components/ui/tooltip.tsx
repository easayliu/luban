"use client";

import { Tooltip as TooltipPrimitive } from "@base-ui/react/tooltip";
import * as React from "react";
import { cn } from "@/lib/utils";

export const TooltipCreateHandle: typeof TooltipPrimitive.createHandle =
  TooltipPrimitive.createHandle;

export const TooltipProvider: typeof TooltipPrimitive.Provider =
  TooltipPrimitive.Provider;

/**
 * base-ui 的 Tooltip 只认鼠标悬停与键盘聚焦（悬停写死 mouseOnly，聚焦只认 :focus-visible），
 * 触屏上点、长按都打不开。这里把开合状态收到本组件，由 TooltipTrigger 补上触屏入口：
 * 点了本身没反应的元素（截断文本、标签）点一下就开；按钮、可点的格子照常执行点击，长按才开。
 * 收起仍交给 base-ui：点外部、再点触发器、Esc。
 */
const TouchOpenContext = React.createContext<{
  open: boolean;
  setOpen: (open: boolean) => void;
} | null>(null);

export function Tooltip({
  onOpenChange,
  ...props
}: Omit<TooltipPrimitive.Root.Props, "open" | "defaultOpen">): React.ReactElement {
  const [open, setOpen] = React.useState(false);
  const context = React.useMemo(() => ({ open, setOpen }), [open]);
  return (
    <TouchOpenContext.Provider value={context}>
      <TooltipPrimitive.Root
        {...props}
        open={open}
        onOpenChange={(next, details) => {
          setOpen(next);
          onOpenChange?.(next, details);
        }}
      />
    </TouchOpenContext.Provider>
  );
}

const TRIGGER_CLASS = "pointer-coarse:[-webkit-touch-callout:none]";
const LONG_PRESS_MS = 500;
const MOVE_TOLERANCE_PX = 10;
/** 触发器自身命中这些就算「可点」：点击留给它本来的动作，提示改为长按。 */
const INTERACTIVE_SELF =
  'a[href],input,select,textarea,[role="switch"],[role="checkbox"],[role="radio"],[role="tab"],[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"],[role="option"],[role="link"],[aria-haspopup],[aria-pressed],[aria-expanded]';
/** 触发器里面或外面套着这些元素（选项里的文字、按钮里的图标），同样把点击留给它们。 */
const INTERACTIVE_NESTED = `${INTERACTIVE_SELF},button,[role="button"]`;

interface TouchPress {
  x: number;
  y: number;
  wasOpen: boolean;
  longPress: boolean;
  fired: boolean;
  timer?: number;
}

export function TooltipTrigger({
  className,
  render,
  onClick,
  onClickCapture,
  onContextMenu,
  onPointerCancel,
  onPointerDown,
  onPointerDownCapture,
  onPointerMove,
  onPointerUp,
  ...props
}: TooltipPrimitive.Trigger.Props): React.ReactElement {
  const touch = React.useContext(TouchOpenContext);
  const press = React.useRef<TouchPress | null>(null);

  const clearTimer = () => {
    if (press.current?.timer !== undefined) {
      window.clearTimeout(press.current.timer);
      press.current.timer = undefined;
    }
  };
  React.useEffect(() => clearTimer, []);

  const renderHasClick =
    React.isValidElement<{ onClick?: unknown }>(render) && render.props.onClick != null;

  return (
    <TooltipPrimitive.Trigger
      data-slot="tooltip-trigger"
      {...props}
      render={render}
      className={
        typeof className === "function"
          ? (state) => cn(TRIGGER_CLASS, className(state))
          : cn(TRIGGER_CLASS, className)
      }
      onPointerDownCapture={(event) => {
        // 先于 base-ui 的「再点触发器收起」记下原状态，下面的点按开关才不会把刚收起的又打开。
        if (touch && event.pointerType === "touch") {
          clearTimer();
          const el = event.currentTarget as HTMLElement;
          const inner = (event.target as Element).closest(INTERACTIVE_NESTED);
          const parent = el.parentElement;
          const longPress =
            onClick != null ||
            renderHasClick ||
            el.matches(INTERACTIVE_SELF) ||
            (inner !== null && inner !== el && el.contains(inner)) ||
            parent?.closest(INTERACTIVE_NESTED) != null ||
            // 挂了 onClick 的 <tr>/<div> 没有语义可查，但都带 cursor-pointer；cursor 会继承，看父元素即可。
            (parent != null && getComputedStyle(parent).cursor === "pointer");
          press.current = {
            x: event.clientX,
            y: event.clientY,
            wasOpen: touch.open,
            longPress,
            fired: false,
          };
        } else {
          press.current = null;
        }
        onPointerDownCapture?.(event);
      }}
      onPointerDown={(event) => {
        const state = press.current;
        if (touch && state?.longPress) {
          state.timer = window.setTimeout(() => {
            state.timer = undefined;
            state.fired = true;
            touch.setOpen(true);
          }, LONG_PRESS_MS);
        }
        onPointerDown?.(event);
      }}
      onPointerMove={(event) => {
        const state = press.current;
        if (
          state &&
          Math.hypot(event.clientX - state.x, event.clientY - state.y) > MOVE_TOLERANCE_PX
        ) {
          clearTimer();
        }
        onPointerMove?.(event);
      }}
      onPointerUp={(event) => {
        clearTimer();
        onPointerUp?.(event);
      }}
      onPointerCancel={(event) => {
        // 页面开始滚动时浏览器发 pointercancel，随后不会有 click，这一按作废。
        clearTimer();
        press.current = null;
        onPointerCancel?.(event);
      }}
      onContextMenu={(event) => {
        // Android 长按会弹系统菜单，与长按看提示冲突。
        if (press.current?.longPress) event.preventDefault();
        onContextMenu?.(event);
      }}
      onClickCapture={(event) => {
        // 长按已经弹出提示，松手时的这次 click 不再触发按钮本来的动作。
        if (press.current?.fired) {
          press.current = null;
          event.preventDefault();
          event.stopPropagation();
          return;
        }
        onClickCapture?.(event);
      }}
      // 始终挂 onClick：React 会给带 onClick 的节点补 onclick，iOS Safari 才会把非交互元素上的点按派发成 click。
      onClick={(event) => {
        const state = press.current;
        press.current = null;
        if (touch && state && !state.longPress) touch.setOpen(!state.wasOpen);
        onClick?.(event);
      }}
    />
  );
}

export function TooltipPopup({
  className,
  align = "center",
  sideOffset = 4,
  side = "top",
  anchor,
  children,
  portalProps,
  ...props
}: TooltipPrimitive.Popup.Props & {
  align?: TooltipPrimitive.Positioner.Props["align"];
  side?: TooltipPrimitive.Positioner.Props["side"];
  sideOffset?: TooltipPrimitive.Positioner.Props["sideOffset"];
  anchor?: TooltipPrimitive.Positioner.Props["anchor"];
  portalProps?: TooltipPrimitive.Portal.Props;
}): React.ReactElement {
  return (
    <TooltipPrimitive.Portal {...portalProps}>
      <TooltipPrimitive.Positioner
        align={align}
        anchor={anchor}
        className="z-50 h-(--positioner-height) w-(--positioner-width) max-w-(--available-width) transition-[top,left,right,bottom,transform] data-instant:transition-none"
        data-slot="tooltip-positioner"
        side={side}
        sideOffset={sideOffset}
      >
        <TooltipPrimitive.Popup
          className={cn(
            "relative flex h-(--popup-height,auto) w-(--popup-width,auto) origin-(--transform-origin) text-balance rounded-md border bg-popover not-dark:bg-clip-padding text-popover-foreground text-xs shadow-md transition-[width,height,scale,opacity] before:pointer-events-none before:absolute before:inset-0 before:rounded-[calc(var(--radius-md)-1px)] before:shadow-[0_1px_var(--bevel)] data-ending-style:scale-98 data-starting-style:scale-98 data-ending-style:opacity-0 data-starting-style:opacity-0 data-instant:duration-0 dark:before:shadow-[0_-1px_var(--bevel)]",
            className,
          )}
          data-slot="tooltip-popup"
          {...props}
        >
          <TooltipPrimitive.Viewport
            className="relative size-full overflow-clip px-(--viewport-inline-padding) py-1 [--viewport-inline-padding:--spacing(2)] data-instant:transition-none **:data-current:data-ending-style:opacity-0 **:data-current:data-starting-style:opacity-0 **:data-previous:data-ending-style:opacity-0 **:data-previous:data-starting-style:opacity-0 **:data-current:w-[calc(var(--popup-width)-2*var(--viewport-inline-padding)-2px)] **:data-previous:w-[calc(var(--popup-width)-2*var(--viewport-inline-padding)-2px)] **:data-previous:truncate **:data-current:opacity-100 **:data-previous:opacity-100 **:data-current:transition-opacity **:data-previous:transition-opacity"
            data-slot="tooltip-viewport"
          >
            {children}
          </TooltipPrimitive.Viewport>
        </TooltipPrimitive.Popup>
      </TooltipPrimitive.Positioner>
    </TooltipPrimitive.Portal>
  );
}

/**
 * 原生 title 的替代：把 children 那个元素原样当作触发器，label 进提示框。
 * label 为空时原样返回 children，条件性的 `title={x ?? undefined}` 可以直接平移过来。
 */
export function Hint({
  label,
  children,
  side,
  className,
}: {
  label: React.ReactNode;
  children: React.ReactElement;
  side?: TooltipPrimitive.Positioner.Props["side"];
  className?: string;
}): React.ReactElement {
  if (label == null || label === false || label === "") return children;
  return (
    <Tooltip>
      <TooltipTrigger render={children} />
      <TooltipPopup
        side={side}
        className={cn("max-w-72 whitespace-pre-line text-left leading-5 [overflow-wrap:anywhere]", className)}
      >
        {label}
      </TooltipPopup>
    </Tooltip>
  );
}

export { TooltipPrimitive, TooltipPopup as TooltipContent };
