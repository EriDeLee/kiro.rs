import { Toaster as Sonner } from 'sonner'

type ToasterProps = React.ComponentProps<typeof Sonner>

/**
 * toast 配色只能走 sonner 自己的 CSS 变量，不能用 className。
 *
 * 两个原因，缺一个都会让暗色模式下的通知发白：
 *
 * 1. sonner 在运行时把样式表注入 <head>，那段 CSS 不属于任何 @layer；
 *    Tailwind v4 把工具类全部编译进 @layer utilities。CSS 层叠规则里
 *    「无层级」优先于「有层级」，与选择器权重无关，所以
 *    `bg-background` 之类的工具类打不过 sonner 的
 *    `[data-sonner-toast]{background:var(--normal-bg)}`。
 * 2. `theme` 默认是 'light'，不显式给的话 --normal-bg 永远是 #fff。
 *    'system' 与 main.tsx 里跟随 prefers-color-scheme 的逻辑一致。
 *
 * 变量内联在 toaster 根节点上（权重高于任何样式表规则），并沿继承
 * 落到每个 toast。取 popover 而不是 background：通知是浮层，要比页面
 * 底色亮一档才有层次。
 */
const TOASTER_VARS = {
  '--normal-bg': 'hsl(var(--popover))',
  '--normal-text': 'hsl(var(--popover-foreground))',
  '--normal-border': 'hsl(var(--border))',
} as React.CSSProperties

const Toaster = ({ style, ...props }: ToasterProps) => {
  return (
    <Sonner
      theme="system"
      style={{ ...TOASTER_VARS, ...style }}
      {...props}
    />
  )
}

export { Toaster }
