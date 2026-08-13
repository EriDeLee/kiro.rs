import * as React from 'react'
import { cn } from '@/lib/utils'

export interface InputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  /**
   * 允许密码管理器接管本字段（自动填充 + 提示保存）。
   *
   * 默认 `false`：本面板绝大多数输入框装的是**上游凭据**（Refresh Token、
   * Kiro API Key、Client Secret、代理密码），不是本站登录凭据。密码管理器把它们
   * 当登录密码处理会造成两种实际损害：把保存过的其它站点账密乱填进来，以及每次
   * 提交都追问“要不要保存密码”。
   *
   * 只有真正的本站登录字段（`login-page` 的管理密钥）该设为 `true` —— 那里用户
   * 通常确实希望密码管理器记住。
   *
   * 默认关闭而非默认开启，是为了让以后新增的机密字段自动继承正确行为：漏写属性
   * 的后果是“少一个便利”，而不是“又一个字段被乱填”。
   */
  allowPasswordManager?: boolean
}

/**
 * 各家密码管理器的退出属性。
 *
 * 没有统一标准，只能逐家列。全部是自定义 `data-*`，对不认识它们的浏览器无副作用。
 * `autoComplete="off"` 单独用不够 —— Chrome 对 `type="password"` 基本无视它，
 * 而第三方扩展根本不读它。
 */
const PASSWORD_MANAGER_OPT_OUT = {
  autoComplete: 'off',
  'data-1p-ignore': '',
  'data-lpignore': 'true',
  'data-bwignore': '',
  'data-protonpass-ignore': '',
  'data-form-type': 'other',
} as const

const Input = React.forwardRef<HTMLInputElement, InputProps>(
  ({ className, type, allowPasswordManager = false, ...props }, ref) => {
    return (
      <input
        type={type}
        // 退出属性放在 `...props` 之前：调用方显式传入的同名属性优先，
        // 例如登录页要传 `autoComplete="current-password"`。
        {...(allowPasswordManager ? {} : PASSWORD_MANAGER_OPT_OUT)}
        className={cn(
          'flex h-10 w-full rounded-xl border border-input bg-background/60 px-3.5 py-2 text-sm ring-offset-background transition-all duration-150 ease-apple',
          'file:border-0 file:bg-transparent file:text-sm file:font-medium file:text-foreground',
          'placeholder:text-muted-foreground/70',
          'hover:border-border focus-visible:outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/30 focus-visible:bg-background',
          'disabled:cursor-not-allowed disabled:opacity-50',
          className
        )}
        ref={ref}
        {...props}
      />
    )
  }
)
Input.displayName = 'Input'

export { Input }
