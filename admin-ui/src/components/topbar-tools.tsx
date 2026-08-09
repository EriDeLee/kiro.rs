import { forwardRef, useEffect, useState, type ComponentPropsWithoutRef } from 'react'
import {
  Activity, RefreshCw, Settings, Key, Wand2, Eye, EyeOff, Copy,
  MoreHorizontal, ShieldAlert, ShieldCheck, Boxes, HeartPulse, HeartCrack,
  Gauge,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent,
  DropdownMenuItem, DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode, useSetLoadBalancingMode,
  useAccountThrottleConfig, useSetAccountThrottleConfig,
  useAccountRpmLimitConfig, useSetAccountRpmLimitConfig,
  useSelfHealConfig, useSetSelfHealConfig,
} from '@/hooks/use-credentials'
import { updateAdminKey, type SelfHealConfigPatch } from '@/api/credentials'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { AvailableModelsDialog } from '@/components/available-models-dialog'

/**
 * 顶栏右侧通用工具栏：负载均衡切换、可用模型、刷新、设置（Key 管理）。
 *
 * 与原 Dashboard 中的工具按钮等价，但全局 Tab 都可访问。刷新按钮会失效
 * 凭据/客户端 Key/统计三类查询，覆盖三个 Tab 的主要数据源。
 */
interface TopbarToolsProps {
  compact?: boolean
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: throttleConfig, isLoading: isLoadingThrottle } = useAccountThrottleConfig()
  const { mutate: setThrottleConfig, isPending: isSettingThrottle } = useSetAccountThrottleConfig()

  const [modelsDialogOpen, setModelsDialogOpen] = useState(false)
  const [keyDialogOpen, setKeyDialogOpen] = useState(false)
  const [newKey, setNewKey] = useState('')
  const [showPlain, setShowPlain] = useState(false)
  const [updating, setUpdating] = useState(false)

  const handleRefresh = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    queryClient.invalidateQueries({ queryKey: ['client-keys'] })
    queryClient.invalidateQueries({ queryKey: ['stats'] })
    queryClient.invalidateQueries({ queryKey: ['current-credential-models'] })
    queryClient.invalidateQueries({ queryKey: ['credential-models'] })
    toast.success('已刷新')
  }

  const handleToggleLoadBalancing = () => {
    const cur = loadBalancingData?.mode || 'priority'
    const next = cur === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(next, {
      onSuccess: () => toast.success(`已切换到${next === 'priority' ? '优先级模式' : '均衡负载模式'}`),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleToggleFailover = () => {
    const cur = throttleConfig?.failover ?? true
    const next = !cur
    setThrottleConfig({ failover: next }, {
      onSuccess: () => toast.success(next ? '已开启账号级风控故障转移' : '已关闭账号级风控故障转移'),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const openKeyDialog = () => {
    setNewKey('')
    setShowPlain(false)
    setKeyDialogOpen(true)
  }

  const handleUpdateKey = async (e: React.FormEvent) => {
    e.preventDefault()
    const key = newKey.trim()
    if (!key) {
      toast.error('新登录API密钥不能为空')
      return
    }
    setUpdating(true)
    try {
      await updateAdminKey({ newKey: key })
      storage.setApiKey(key)
      toast.success('登录API密钥已更新，已自动切换到新 Key')
      setKeyDialogOpen(false)
      setNewKey('')
    } catch (err) {
      toast.error(`更新失败: ${extractErrorMessage(err)}`)
    } finally {
      setUpdating(false)
    }
  }

  const controls = {
    handleRefresh,
    handleToggleFailover,
    handleToggleLoadBalancing,
    isLoadingMode,
    isLoadingThrottle,
    isSettingMode,
    isSettingThrottle,
    loadBalancingMode: loadBalancingData?.mode,
    openModels: () => setModelsDialogOpen(true),
    openKeyDialog,
    throttleConfig,
    updateCooldown: (secs: number) =>
      setThrottleConfig({ cooldownSecs: secs }, {
        onSuccess: () =>
          toast.success(`冷却时长已设为 ${Math.round(secs / 60)} 分钟`),
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      }),
  }

  return (
    <>
      {compact ? <CompactTools controls={controls} /> : <FullTools controls={controls} />}
      <AvailableModelsDialog
        open={modelsDialogOpen}
        onOpenChange={setModelsDialogOpen}
      />

      <Dialog
        open={keyDialogOpen}
        onOpenChange={(open) => { if (!updating) setKeyDialogOpen(open) }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Key className="h-4 w-4" />
              修改登录API密钥
            </DialogTitle>
            <DialogDescription>
              用于登录此管理面板。修改后将自动更新本地存储的 Key，无需重新登录。
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleUpdateKey} className="space-y-4 py-2">
            <div className="relative">
              <Input
                type={showPlain ? 'text' : 'password'}
                placeholder="输入或生成新的登录API密钥"
                value={newKey}
                onChange={(e) => setNewKey(e.target.value)}
                disabled={updating}
                autoFocus
                className="pr-20 font-mono text-[13px]"
              />
              <div className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-1.5">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={() => setShowPlain((v) => !v)}
                  disabled={updating}
                  title={showPlain ? '隐藏' : '显示'}
                >
                  {showPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={async () => {
                    if (!newKey.trim()) {
                      toast.error('请先输入或生成 Key 再复制')
                      return
                    }
                    try {
                      await navigator.clipboard.writeText(newKey)
                      toast.success('已复制到剪贴板')
                    } catch {
                      toast.error('复制失败，请手动选择文本')
                    }
                  }}
                  disabled={updating}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  const key = generateApiKey('sk-admin-')
                  setNewKey(key)
                  setShowPlain(true)
                }}
                disabled={updating}
              >
                <Wand2 className="h-3.5 w-3.5" />生成随机 Key
              </Button>
              <p className="text-[11px] text-muted-foreground">
                建议生成后立即复制保存，确认更新后即生效。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setKeyDialogOpen(false)} disabled={updating}>
                取消
              </Button>
              <Button type="submit" disabled={updating || !newKey.trim()}>
                {updating ? '更新中…' : '确认更新'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  )
}

interface ToolControls {
  handleRefresh: () => void
  handleToggleFailover: () => void
  handleToggleLoadBalancing: () => void
  isLoadingMode: boolean
  isLoadingThrottle: boolean
  isSettingMode: boolean
  isSettingThrottle: boolean
  loadBalancingMode?: 'priority' | 'balanced'
  openKeyDialog: () => void
  openModels: () => void
  throttleConfig?: { failover: boolean; cooldownSecs: number }
  updateCooldown: (secs: number) => void
}

function FullTools({ controls }: { controls: ToolControls }) {
  return (
    <>
      <LoadBalancingButton controls={controls} />
      <ThrottleConfigButton
        config={controls.throttleConfig}
        loading={controls.isLoadingThrottle}
        saving={controls.isSettingThrottle}
        onToggleFailover={controls.handleToggleFailover}
        onChangeCooldown={controls.updateCooldown}
      />
      <SelfHealConfigButton />
      <AccountRpmLimitButton />
      <ModelsButton onOpen={controls.openModels} />
      <RefreshButton onRefresh={controls.handleRefresh} />
      <KeySettingsMenu onOpenKeyDialog={controls.openKeyDialog} />
    </>
  )
}

function CompactTools({ controls }: { controls: ToolControls }) {
  const throttleProps = {
    config: controls.throttleConfig,
    loading: controls.isLoadingThrottle,
    saving: controls.isSettingThrottle,
    onToggleFailover: controls.handleToggleFailover,
    onChangeCooldown: controls.updateCooldown,
  }

  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="icon" title="更多操作">
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="max-h-[calc(100dvh-4.5rem)] w-72 max-w-[calc(100dvw-1rem)] overflow-x-hidden overflow-y-auto overscroll-contain"
      >
        <DropdownMenuLabel>系统操作</DropdownMenuLabel>
        <DropdownMenuItem
          disabled={controls.isLoadingMode || controls.isSettingMode}
          onSelect={controls.handleToggleLoadBalancing}
        >
          <Activity />
          {controls.isLoadingMode
            ? '负载均衡加载中'
            : controls.loadBalancingMode === 'priority'
              ? '切换到均衡负载'
              : '切换到优先级'}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.handleRefresh}>
          <RefreshCw />刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openModels}>
          <Boxes />可用模型
        </DropdownMenuItem>
        <ThrottleCompactItems {...throttleProps} />
        <SelfHealCompactItems />
        <AccountRpmLimitCompactItems />
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <DropdownMenuItem onSelect={controls.openKeyDialog}>
          <Key />修改登录API密钥（管理面板登录）
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function LoadBalancingButton({ controls }: { controls: ToolControls }) {
  return (
    <Button
      variant="outline"
      size="sm"
      onClick={controls.handleToggleLoadBalancing}
      disabled={controls.isLoadingMode || controls.isSettingMode}
      title="切换负载均衡模式"
    >
      <Activity className="h-3.5 w-3.5" />
      <span className="hidden md:inline">
        {controls.isLoadingMode
          ? '加载中…'
          : controls.loadBalancingMode === 'priority'
            ? '优先级'
            : '均衡负载'}
      </span>
    </Button>
  )
}

function ModelsButton({ onOpen }: { onOpen: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onOpen} title="可用模型">
      <Boxes className="h-4 w-4" />
    </Button>
  )
}

function RefreshButton({ onRefresh }: { onRefresh: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新">
      <RefreshCw className="h-4 w-4" />
    </Button>
  )
}

function KeySettingsMenu({ onOpenKeyDialog }: { onOpenKeyDialog: () => void }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="设置">
          <Settings className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <DropdownMenuItem onSelect={onOpenKeyDialog}>
          <Key />修改登录API密钥（管理面板登录）
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

interface ThrottleConfigButtonProps {
  config?: { failover: boolean; cooldownSecs: number }
  loading: boolean
  saving: boolean
  onToggleFailover: () => void
  onChangeCooldown: (secs: number) => void
}

interface ThrottleState {
  cooldownMin: number
  cooldownSecs: number
  failover: boolean
}

interface CustomCooldownFormProps {
  cooldownMin: number
  customMin: string
  disabled: boolean
  onCustomMinChange: (value: string) => void
  onSubmit: (e: React.FormEvent) => void
}

interface ThrottleTriggerProps extends ComponentPropsWithoutRef<typeof Button> {
  loading: boolean
  saving: boolean
  state: ThrottleState
}

const COOLDOWN_PRESETS = [
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
  { label: '1 小时', secs: 60 * 60 },
  { label: '2 小时', secs: 2 * 60 * 60 },
]

const DEFAULT_COOLDOWN_SECS = 30 * 60
const SECONDS_PER_MINUTE = 60
const MIN_CUSTOM_COOLDOWN_MINUTES = 1
const MAX_CUSTOM_COOLDOWN_MINUTES = 1440

/**
 * 故障转移开关 + 冷却时长设置（紧凑下拉）
 *
 * 主按钮文案显示当前状态；下拉里:
 * - 顶部一个 Switch 切换 failover
 * - 5 个预设时长 + 一个自定义输入（分钟）
 */
function ThrottleConfigButton({
  config, loading, saving, onToggleFailover, onChangeCooldown,
}: ThrottleConfigButtonProps) {
  const [open, setOpen] = useState(false)
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(config)

  useEffect(() => {
    if (!open) setCustomMin('')
  }, [open])

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setOpen(false)
  }

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <ThrottleTrigger loading={loading} saving={saving} state={state} />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <ThrottleStatusPanel
          saving={saving}
          state={state}
          onToggleFailover={onToggleFailover}
        />
        <ThrottleCooldownPanel
          customMin={customMin}
          saving={saving}
          state={state}
          onChangeCooldown={onChangeCooldown}
          onCustomMinChange={setCustomMin}
          onDone={() => setOpen(false)}
          onSubmitCustom={submitCustom}
        />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

const ThrottleTrigger = forwardRef<HTMLButtonElement, ThrottleTriggerProps>(
  function ThrottleTrigger({ loading, saving, state, ...props }, ref) {
    return (
      <Button
        {...props}
        ref={ref}
        variant="outline"
        size="sm"
        disabled={loading || saving}
        title={throttleTitle(loading, state)}
      >
        {state.failover ? (
          <ShieldCheck className="h-3.5 w-3.5 text-emerald-600" />
        ) : (
          <ShieldAlert className="h-3.5 w-3.5 text-amber-500" />
        )}
        <span className="hidden md:inline">
          {throttleTriggerText(loading, state)}
        </span>
      </Button>
    )
  },
)

function ThrottleStatusPanel({
  saving, state, onToggleFailover,
}: {
  saving: boolean
  state: ThrottleState
  onToggleFailover: () => void
}) {
  return (
    <>
      <DropdownMenuLabel>账号级风控故障转移</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <ThrottleStatusText failover={state.failover} />
          <Switch
            checked={state.failover}
            disabled={saving}
            onCheckedChange={() => onToggleFailover()}
          />
        </div>
      </div>
    </>
  )
}

function ThrottleStatusText({ failover }: { failover: boolean }) {
  return (
    <div className="text-xs">
      <div className="font-medium text-foreground">
        {failover ? '开启' : '关闭'}
      </div>
      <div className="text-muted-foreground leading-snug">
        {failover
          ? '上游对当前账号触发临时限速时，自动冷却该凭据并切换到下一个可用凭据'
          : '上游对当前账号触发临时限速时，仅按瞬态错误重试，不切换凭据'}
      </div>
    </div>
  )
}

function ThrottleCooldownPanel({
  customMin, saving, state, onChangeCooldown, onCustomMinChange, onDone, onSubmitCustom,
}: {
  customMin: string
  saving: boolean
  state: ThrottleState
  onChangeCooldown: (secs: number) => void
  onCustomMinChange: (value: string) => void
  onDone?: () => void
  onSubmitCustom: (e: React.FormEvent) => void
}) {
  const disabled = saving || !state.failover

  return (
    <>
      <DropdownMenuLabel className="pt-1">冷却时长</DropdownMenuLabel>
      <div className={cooldownPanelClassName(state.failover)}>
        <CooldownPresetButtons
          cooldownSecs={state.cooldownSecs}
          disabled={disabled}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
        <CustomCooldownForm
          cooldownMin={state.cooldownMin}
          customMin={customMin}
          disabled={disabled}
          onCustomMinChange={onCustomMinChange}
          onSubmit={onSubmitCustom}
        />
      </div>
    </>
  )
}

function CustomCooldownForm({
  cooldownMin, customMin, disabled, onCustomMinChange, onSubmit,
}: CustomCooldownFormProps) {
  return (
    <form onSubmit={onSubmit} className="mt-2 flex items-center gap-1.5">
      <Input
        type="number"
        min={MIN_CUSTOM_COOLDOWN_MINUTES}
        max={MAX_CUSTOM_COOLDOWN_MINUTES}
        placeholder={`自定义（当前 ${cooldownMin}）`}
        value={customMin}
        onChange={(e) => onCustomMinChange(e.target.value)}
        disabled={disabled}
        className="h-7 text-xs"
      />
      <span className="text-xs text-muted-foreground">分钟</span>
      <Button
        type="submit"
        size="sm"
        variant="outline"
        className="h-7 text-xs"
        disabled={disabled || !customMin.trim()}
      >
        保存
      </Button>
    </form>
  )
}

function ThrottleCompactItems(props: ThrottleConfigButtonProps) {
  const { loading, saving, onToggleFailover, onChangeCooldown } = props
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(props.config)
  const busy = loading || saving

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setCustomMin('')
  }

  return (
    <>
      <DropdownMenuLabel>故障转移</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={onToggleFailover}
      >
        {state.failover ? <ShieldCheck /> : <ShieldAlert />}
        {compactThrottleText(loading, state)}
      </DropdownMenuItem>
      <ThrottleCooldownPanel
        customMin={customMin}
        saving={busy}
        state={state}
        onChangeCooldown={onChangeCooldown}
        onCustomMinChange={setCustomMin}
        onSubmitCustom={submitCustom}
      />
    </>
  )
}

// ============ 自愈治理 ============

const SELF_HEAL_INTERVAL_PRESETS = [
  { label: '不冷却', secs: 0 },
  { label: '1 分钟', secs: 60 },
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
]

/**
 * 自愈治理设置（下拉）：
 * - 开关：是否启用凭据自愈
 * - 冷却间隔：两次自愈的最小间隔（打断持续 403 死循环的关键）
 * - 连续上限：连续自愈达到该轮数且期间无成功则停止（0=不限）
 * - 只读观测：凭据最大连续轮数 / 累计恢复凭据次数
 */
function useSelfHealPanelState(resetInput: boolean) {
  const { data: config, isLoading } = useSelfHealConfig()
  const { mutate, isPending } = useSetSelfHealConfig()
  const [roundsInput, setRoundsInput] = useState('')

  useEffect(() => {
    if (resetInput) setRoundsInput('')
  }, [resetInput])

  const enabled = config?.enabled ?? true
  const busy = isLoading || isPending

  const save = (patch: SelfHealConfigPatch, msg: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(msg),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  const submitRounds = (e: React.FormEvent) => {
    e.preventDefault()
    const n = parseInt(roundsInput, 10)
    if (Number.isNaN(n) || n < 0 || n > 1000) {
      toast.error('请输入 0-1000 之间的轮数（0=不限）')
      return
    }
    save({ maxConsecutiveRounds: n }, n === 0 ? '连续自愈已设为不限' : `连续自愈上限已设为 ${n} 轮`)
    setRoundsInput('')
  }

  return {
    busy,
    config,
    enabled,
    isLoading,
    roundsInput,
    save,
    setRoundsInput,
    submitRounds,
  }
}

type SelfHealPanelState = ReturnType<typeof useSelfHealPanelState>

function SelfHealConfigButton() {
  const [open, setOpen] = useState(false)
  const panel = useSelfHealPanelState(!open)

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          disabled={panel.busy}
          title={panel.enabled ? '凭据自愈：已启用' : '凭据自愈：已关闭'}
        >
          {panel.enabled ? (
            <HeartPulse className="h-3.5 w-3.5 text-emerald-600" />
          ) : (
            <HeartCrack className="h-3.5 w-3.5 text-amber-500" />
          )}
          <span className="hidden md:inline">
            {panel.isLoading ? '自愈…' : panel.enabled ? '自愈开' : '自愈关'}
          </span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-72">
        <SelfHealConfigPanel {...panel} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function SelfHealConfigPanel({
  busy,
  config,
  enabled,
  roundsInput,
  save,
  setRoundsInput,
  submitRounds,
}: SelfHealPanelState) {
  return (
    <>
      <DropdownMenuLabel>凭据自愈</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">{enabled ? '已启用' : '已关闭'}</div>
            <div className="text-muted-foreground">
              当前请求池全灭时按作用域恢复凭据
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy}
            onCheckedChange={(v) => save({ enabled: v }, v ? '已开启凭据自愈' : '已关闭凭据自愈')}
          />
        </div>
        {config && (
          <div className="mt-2 flex items-center justify-between rounded-md bg-secondary/20 px-2.5 py-1.5 text-xs text-muted-foreground">
            <span>连续 {config.consecutiveRounds} 轮</span>
            <span>累计恢复 {config.totalCount} 次</span>
          </div>
        )}
      </div>

      <DropdownMenuLabel className="pt-1">403 封禁识别</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">
              {config?.suspendedDetectionEnabled ?? true ? '已启用' : '已关闭'}
            </div>
            <div className="text-muted-foreground">
              命中封禁文案的 403 立即禁用，不参与自愈
            </div>
          </div>
          <Switch
            checked={config?.suspendedDetectionEnabled ?? true}
            disabled={busy}
            onCheckedChange={(v) =>
              save({ suspendedDetectionEnabled: v }, v ? '已开启 403 封禁识别' : '已关闭 403 封禁识别')
            }
          />
        </div>
      </div>

      <DropdownMenuLabel className="pt-1">自愈冷却间隔</DropdownMenuLabel>
      <div className={cooldownPanelClassName(enabled)}>
        <div className="grid grid-cols-3 gap-1.5">
          {SELF_HEAL_INTERVAL_PRESETS.map((p) => (
            <Button
              key={p.secs}
              size="sm"
              variant={config?.minIntervalSecs === p.secs ? 'default' : 'outline'}
              className="h-7 text-xs"
              disabled={busy || !enabled}
              onClick={() => save({ minIntervalSecs: p.secs }, `自愈冷却已设为「${p.label}」`)}
            >
              {p.label}
            </Button>
          ))}
        </div>

        <DropdownMenuLabel className="px-0 pt-2">连续自愈上限（0=不限）</DropdownMenuLabel>
        <form onSubmit={submitRounds} className="mt-1 flex items-center gap-1.5">
          <Input
            type="number"
            min={0}
            max={1000}
            placeholder={`当前 ${config?.maxConsecutiveRounds ?? 5} 轮`}
            value={roundsInput}
            onChange={(e) => setRoundsInput(e.target.value)}
            disabled={busy || !enabled}
            className="h-7 min-w-0 text-xs"
          />
          <span className="text-xs text-muted-foreground">轮</span>
          <Button
            type="submit"
            size="sm"
            variant="outline"
            className="h-7 text-xs"
            disabled={busy || !enabled || !roundsInput.trim()}
          >
            保存
          </Button>
        </form>
      </div>
    </>
  )
}

/** 紧凑菜单复用完整配置，避免移动端丢失治理选项。 */
function SelfHealCompactItems() {
  const panel = useSelfHealPanelState(false)
  return <SelfHealConfigPanel {...panel} />
}

const RPM_LIMIT_PRESETS = [10, 30, 60, 120, 300]
const MIN_RPM_LIMIT = 1
const MAX_RPM_LIMIT = 100000

/**
 * 单账号 RPM 主动限流：开关 + 每分钟上限设置（紧凑下拉）。
 *
 * 开启后每个账号独立维护 60 秒滑动窗口，达到上限时该账号被临时排除出候选，
 * 请求自动故障转移到下一个可用账号；全部超限时返回 429。
 */
function useAccountRpmLimitPanelState(resetInput: boolean) {
  const { data: config, isLoading, isError } = useAccountRpmLimitConfig()
  const { mutate, isPending } = useSetAccountRpmLimitConfig()
  const [limitInput, setLimitInput] = useState('')

  useEffect(() => {
    if (resetInput) setLimitInput('')
  }, [resetInput])

  const enabled = config?.enabled ?? false
  const limit = config?.limit ?? 60
  // 读取失败时 config 为 undefined，上面两个兜底值会把「取不到」渲染成「已关闭·60」。
  // 那是在替服务器编状态，所以读取失败一并按 busy 处理并显式提示，不给可点的控件。
  const busy = isLoading || isPending || isError

  // 成功提示用服务器回显的生效值，而不是请求里带的值：后端可能因为「与现值相同」
  // 而短路，也可能只应用了 patch 里的一个字段。
  const save = (
    patch: { enabled?: boolean; limit?: number },
    msg: (applied: { enabled: boolean; limit: number }) => string,
  ) => {
    mutate(patch, {
      onSuccess: (applied) => toast.success(msg(applied)),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  const submitLimit = (e: React.FormEvent) => {
    e.preventDefault()
    // 用 Number 而非 parseInt：number input 允许 "1e3" / "60.5" 这类合法值，
    // parseInt 会把它们截成 1 / 60 后照常提交，等于静默改写用户输入的档位。
    // 这里一律拒绝并把原样输入回显在报错里，不做任何取整或夹取。
    const n = Number(limitInput.trim())
    if (
      !Number.isInteger(n) ||
      n < MIN_RPM_LIMIT ||
      n > MAX_RPM_LIMIT
    ) {
      toast.error(`请输入 ${MIN_RPM_LIMIT}-${MAX_RPM_LIMIT} 之间的整数（当前：${limitInput}）`)
      return
    }
    save({ limit: n }, (a) => `单账号 RPM 上限已设为 ${a.limit} 次/分钟`)
    setLimitInput('')
  }

  return {
    busy,
    enabled,
    isError,
    isLoading,
    limit,
    limitInput,
    save,
    setLimitInput,
    submitLimit,
  }
}

type AccountRpmLimitPanelState = ReturnType<typeof useAccountRpmLimitPanelState>

function AccountRpmLimitButton() {
  const [open, setOpen] = useState(false)
  const panel = useAccountRpmLimitPanelState(!open)

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          disabled={panel.busy}
          title={panel.enabled ? `单账号限流：${panel.limit} 次/分钟` : '单账号限流：已关闭'}
        >
          <Gauge className={panel.enabled ? 'h-3.5 w-3.5 text-emerald-600' : 'h-3.5 w-3.5 text-muted-foreground'} />
          <span className="hidden md:inline">
            {panel.isLoading ? '限流…' : panel.enabled ? `限流 ${panel.limit}/分` : '限流关'}
          </span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-72">
        <AccountRpmLimitPanel {...panel} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function AccountRpmLimitPanel({
  busy,
  enabled,
  isError,
  limit,
  limitInput,
  save,
  setLimitInput,
  submitLimit,
}: AccountRpmLimitPanelState) {
  return (
    <>
      <DropdownMenuLabel>单账号每分钟请求限流</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">
              {isError ? '读取失败' : enabled ? '已启用' : '已关闭'}
            </div>
            <div className="text-muted-foreground leading-snug">
              {isError
                ? '拉取限流配置失败，当前状态未知；请刷新页面重试'
                : '单账号超过每分钟上限时临时跳过并切换到下一个可用账号；全部超限则返回 429'}
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy}
            onCheckedChange={(v) =>
              save({ enabled: v }, (a) =>
                a.enabled ? '已开启单账号限流' : '已关闭单账号限流',
              )
            }
          />
        </div>
      </div>

      <DropdownMenuLabel className="pt-1">每分钟上限</DropdownMenuLabel>
      <div className={cooldownPanelClassName(enabled)}>
        <div className="grid grid-cols-3 gap-1.5">
          {RPM_LIMIT_PRESETS.map((n) => (
            <Button
              key={n}
              size="sm"
              variant={limit === n ? 'default' : 'outline'}
              className="h-7 text-xs"
              disabled={busy || !enabled}
              // 与 CooldownPresetButton 一致：点当前值不再发一次注定短路的 PUT，
              // 否则后端原样返回而 toast 仍报「已设为」，看着像做了什么
              onClick={() => {
                if (limit === n) return
                save({ limit: n }, (a) => `单账号 RPM 上限已设为 ${a.limit} 次/分钟`)
              }}
            >
              {n}
            </Button>
          ))}
        </div>

        <DropdownMenuLabel className="px-0 pt-2">自定义（次/分钟）</DropdownMenuLabel>
        <form onSubmit={submitLimit} className="mt-1 flex items-center gap-1.5">
          <Input
            type="number"
            min={MIN_RPM_LIMIT}
            max={MAX_RPM_LIMIT}
            placeholder={`当前 ${limit} 次`}
            value={limitInput}
            onChange={(e) => setLimitInput(e.target.value)}
            disabled={busy || !enabled}
            className="h-7 min-w-0 text-xs"
          />
          <span className="text-xs text-muted-foreground">次</span>
          <Button
            type="submit"
            size="sm"
            variant="outline"
            className="h-7 text-xs"
            disabled={busy || !enabled || !limitInput.trim()}
          >
            保存
          </Button>
        </form>
      </div>
    </>
  )
}

/** 紧凑菜单复用完整配置，避免移动端只能切换开关。 */
function AccountRpmLimitCompactItems() {
  const panel = useAccountRpmLimitPanelState(false)
  return <AccountRpmLimitPanel {...panel} />
}

function CooldownPresetButtons({
  cooldownSecs, disabled, onChangeCooldown, onDone,
}: {
  cooldownSecs: number
  disabled: boolean
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <div className="grid grid-cols-3 gap-1">
      {COOLDOWN_PRESETS.map((preset) => (
        <CooldownPresetButton
          key={preset.secs}
          active={preset.secs === cooldownSecs}
          disabled={disabled}
          label={preset.label}
          secs={preset.secs}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
      ))}
    </div>
  )
}

function CooldownPresetButton({
  active, disabled, label, secs, onChangeCooldown, onDone,
}: {
  active: boolean
  disabled: boolean
  label: string
  secs: number
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? 'default' : 'outline'}
      className="h-7 text-xs"
      disabled={disabled}
      onClick={() => {
        if (!active) onChangeCooldown(secs)
        onDone?.()
      }}
    >
      {label}
    </Button>
  )
}

function secondsToMinutes(seconds: number) {
  return Math.round(seconds / SECONDS_PER_MINUTE)
}

function readThrottleState(
  config: ThrottleConfigButtonProps['config'],
): ThrottleState {
  const cooldownSecs = config?.cooldownSecs ?? DEFAULT_COOLDOWN_SECS
  return {
    cooldownMin: secondsToMinutes(cooldownSecs),
    cooldownSecs,
    failover: config?.failover ?? true,
  }
}

function throttleTitle(loading: boolean, state: ThrottleState) {
  if (loading) return '加载中…'
  if (!state.failover) return '账号级风控故障转移：关闭'
  return `账号级风控故障转移：开启（冷却 ${state.cooldownMin} 分钟）`
}

function throttleTriggerText(loading: boolean, state: ThrottleState) {
  if (loading) return '加载中…'
  if (!state.failover) return '不切换'
  return `故障转移 · ${state.cooldownMin}m`
}

function compactThrottleText(loading: boolean, state: ThrottleState) {
  if (loading) return '故障转移加载中'
  if (!state.failover) return '开启故障转移'
  return `关闭故障转移 · ${state.cooldownMin}m`
}

function invalidCooldownMinutes(minutes: number) {
  return (
    Number.isNaN(minutes) ||
    minutes < MIN_CUSTOM_COOLDOWN_MINUTES ||
    minutes > MAX_CUSTOM_COOLDOWN_MINUTES
  )
}

function cooldownPanelClassName(failover: boolean) {
  return `px-2 pb-2 ${failover ? '' : 'opacity-60'}`
}
