import * as React from 'react'
import { Slot } from '@radix-ui/react-slot'
import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'inline-flex cursor-pointer items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium ring-offset-background transition-[color,background-color,border-color,box-shadow] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30 disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0',
  {
    variants: {
      variant: {
        default: 'bg-primary text-primary-foreground hover:bg-primary/90 active:bg-primary/80 data-[state=open]:bg-primary/90',
        destructive: 'bg-destructive text-destructive-foreground hover:brightness-110 active:brightness-95 data-[state=open]:brightness-110',
        outline: 'border border-border bg-transparent hover:bg-muted hover:text-foreground active:bg-muted/80 aria-pressed:bg-muted aria-pressed:text-foreground data-[state=open]:bg-muted data-[state=open]:text-foreground',
        secondary: 'bg-muted text-foreground hover:bg-muted/80 active:bg-muted/70 aria-pressed:bg-muted/80 data-[state=open]:bg-muted/80',
        ghost: 'text-muted-foreground hover:bg-muted hover:text-foreground active:bg-muted/80 aria-pressed:bg-muted aria-pressed:text-foreground data-[state=open]:bg-muted data-[state=open]:text-foreground',
        link: 'text-foreground underline-offset-4 hover:underline active:text-foreground/70',
      },
      size: {
        default: 'h-9 px-4 py-2',
        sm: 'h-8 px-3',
        lg: 'h-10 px-8',
        icon: 'size-9',
      },
    },
    defaultVariants: {
      variant: 'default',
      size: 'default',
    },
  },
)

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, ...props }, ref) => {
    const Comp = asChild ? Slot : 'button'
    return <Comp className={cn(buttonVariants({ variant, size, className }))} ref={ref} {...props} />
  },
)
Button.displayName = 'Button'

export { Button, buttonVariants }
