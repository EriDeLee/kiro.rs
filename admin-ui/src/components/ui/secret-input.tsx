import * as React from 'react'
import { Eye, EyeOff } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Input, type InputProps } from '@/components/ui/input'
import { cn } from '@/lib/utils'

export interface SecretInputProps extends Omit<InputProps, 'type' | 'allowPasswordManager'> {
  /** 眼睛按钮的无障碍标签前缀，例如 "Refresh Token"。 */
  secretLabel?: string
}

/**
 * 机密字段输入框：默认遮罩、可点开查看，且**不被密码管理器当成登录密码**。
 *
 * 关键实现选择：`type` 恒为 `text`，遮罩由 CSS `-webkit-text-security` 完成，
 * 绝不用 `type="password"`。
 *
 * 原因是 Chrome 判定“这是不是一个登录表单”主要看有没有 `type="password"` 字段，
 * 而不看 `autocomplete`。只要还是密码框，提交时就会追问“要不要保存密码”，各家扩展
 * 也会继续往里灌保存过的账密。本面板这些字段装的是上游凭据（Refresh Token、
 * Kiro API Key、Client Secret、代理密码），不是本站登录凭据，被当成后者纯属误伤。
 *
 * 代价：`-webkit-text-security` 在 Chrome / Edge / Safari 均支持，Firefox 132 起
 * 支持；更早的 Firefox 上遮罩会失效、内容明文可见。因此**不能**把它当安全边界 ——
 * 它只是防旁人扫一眼，真正的保护在于后端从不回传这些值（列表接口只给哈希与掩码）。
 */
const SecretInput = React.forwardRef<HTMLInputElement, SecretInputProps>(
  ({ className, secretLabel, disabled, ...props }, ref) => {
    const [revealed, setRevealed] = React.useState(false)

    return (
      <div className="relative">
        <Input
          {...props}
          ref={ref}
          type="text"
          disabled={disabled}
          className={cn(
            'pr-10 font-mono',
            // 遮罩用任意属性写法，避免为一行 CSS 单开全局样式表条目。
            // `text-security` 无前缀版本尚未落地，故只写 `-webkit-` 前缀版本。
            !revealed && '[-webkit-text-security:disc]',
            className
          )}
        />
        <div className="absolute inset-y-0 right-0 flex items-center pr-1">
          <Button
            type="button"
            size="icon"
            variant="ghost"
            className="h-7 w-7"
            disabled={disabled}
            onClick={() => setRevealed((v) => !v)}
            // 图标按钮没有可见文字，必须给无障碍名称；同时用 title 提供悬浮提示。
            aria-label={`${revealed ? '隐藏' : '显示'}${secretLabel ? ` ${secretLabel}` : ''}`}
            aria-pressed={revealed}
            title={revealed ? '隐藏' : '显示'}
          >
            {revealed ? (
              <EyeOff className="h-3.5 w-3.5" aria-hidden="true" />
            ) : (
              <Eye className="h-3.5 w-3.5" aria-hidden="true" />
            )}
          </Button>
        </div>
      </div>
    )
  }
)
SecretInput.displayName = 'SecretInput'

export { SecretInput }
