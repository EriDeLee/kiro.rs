import { useEffect, useRef, useState } from 'react'
import {
  AlertCircle,
  Boxes,
  CheckCircle2,
  Clock3,
  Loader2,
  Play,
  RefreshCw,
} from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  useCredentialModels,
  useCurrentCredentialModels,
  useTestModel,
} from '@/hooks/use-credentials'
import { formatNumber, parseError } from '@/lib/utils'
import type {
  AvailableModelItem,
  AvailableModelsResponse,
  ModelTestResponse,
} from '@/types/api'

interface AvailableModelsDialogProps {
  credentialId?: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

type ModelTestResult =
  | { status: 'success'; data: ModelTestResponse }
  | { status: 'error'; message: string }

function formatSelectionSource(data: AvailableModelsResponse) {
  switch (data.selectionMode) {
    case 'specified':
      return `指定凭据 #${data.id}`
    // 模式名与顶栏切换按钮、切换成功提示逐字一致：优先级模式 / 均衡负载模式
    case 'priority':
      return `优先级模式选中凭据 #${data.id}`
    case 'balanced':
      return `均衡负载模式选中凭据 #${data.id}`
  }
}

export function AvailableModelsDialog({
  credentialId,
  open,
  onOpenChange,
}: AvailableModelsDialogProps) {
  const fixedCredentialId = typeof credentialId === 'number' ? credentialId : null
  const credentialQuery = useCredentialModels(open ? fixedCredentialId : null)
  const currentQuery = useCurrentCredentialModels(open && fixedCredentialId === null)
  const query = fixedCredentialId === null ? currentQuery : credentialQuery
  const testMutation = useTestModel()
  const requestSequence = useRef(0)
  const [testingModelId, setTestingModelId] = useState<string | null>(null)
  const [testResults, setTestResults] = useState<Record<string, ModelTestResult>>({})

  useEffect(() => {
    requestSequence.current += 1
    setTestingModelId(null)
    setTestResults({})
  }, [open, fixedCredentialId])

  const handleTest = async (modelId: string) => {
    const sequence = ++requestSequence.current
    setTestingModelId(modelId)
    setTestResults((current) => {
      const next = { ...current }
      delete next[modelId]
      return next
    })

    try {
      const data = await testMutation.mutateAsync(modelId)
      if (requestSequence.current !== sequence) return
      setTestResults((current) => ({
        ...current,
        [modelId]: { status: 'success', data },
      }))
    } catch (error) {
      if (requestSequence.current !== sequence) return
      const parsed = parseError(error)
      const message = parsed.detail
        ? `${parsed.title}: ${parsed.detail}`
        : parsed.title
      setTestResults((current) => ({
        ...current,
        [modelId]: { status: 'error', message },
      }))
    } finally {
      if (requestSequence.current === sequence) setTestingModelId(null)
    }
  }

  const handleRefresh = () => {
    requestSequence.current += 1
    setTestingModelId(null)
    setTestResults({})
    void query.refetch()
  }

  const title = fixedCredentialId === null
    ? '账号池可用模型'
    : `凭据 #${fixedCredentialId} 可用模型`

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[calc(100dvh-2rem)] grid-rows-[auto_minmax(0,1fr)] overflow-hidden sm:max-w-2xl">
        <DialogHeader className="pr-8">
          <div className="flex min-w-0 items-center justify-between gap-3">
            <DialogTitle className="flex min-w-0 items-center gap-2">
              <Boxes className="h-4 w-4 shrink-0" />
              <span className="truncate">{title}</span>
            </DialogTitle>
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="h-8 w-8 shrink-0"
              disabled={query.isFetching || testingModelId !== null}
              onClick={handleRefresh}
              title="刷新模型列表"
            >
              <RefreshCw className={query.isFetching ? 'animate-spin' : ''} />
            </Button>
          </div>
          {query.data && (
            <div className="text-xs text-muted-foreground">
              {formatSelectionSource(query.data)}，共 {query.data.models.length} 个模型
            </div>
          )}
        </DialogHeader>

        <div className="min-h-0 overflow-y-auto pr-1">
          {query.isLoading && !query.data && (
            <div className="flex items-center justify-center py-10 text-muted-foreground">
              <Loader2 className="h-7 w-7 animate-spin" />
              <span className="sr-only">正在加载模型</span>
            </div>
          )}

          {query.error && <QueryError error={query.error} />}

          {query.data && query.data.models.length === 0 && (
            <div className="py-10 text-center text-sm text-muted-foreground">
              该凭据当前没有可用模型
            </div>
          )}

          {query.data && query.data.models.length > 0 && (
            <div className="space-y-2">
              {query.data.models.map((model) => (
                <ModelRow
                  key={model.modelId}
                  model={model}
                  result={testResults[model.modelId]}
                  testing={testingModelId === model.modelId}
                  testDisabled={testingModelId !== null}
                  onTest={() => handleTest(model.modelId)}
                />
              ))}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}

function QueryError({ error }: { error: unknown }) {
  const parsed = parseError(error)

  return (
    <div className="space-y-2 py-8 text-center">
      <div className="flex items-center justify-center gap-2 font-medium text-destructive">
        <AlertCircle className="h-5 w-5" />
        <span>{parsed.title}</span>
      </div>
      {parsed.detail && (
        <div className="px-4 text-sm text-muted-foreground">{parsed.detail}</div>
      )}
    </div>
  )
}

function ModelRow({
  model,
  result,
  testing,
  testDisabled,
  onTest,
}: {
  model: AvailableModelItem
  result?: ModelTestResult
  testing: boolean
  testDisabled: boolean
  onTest: () => void
}) {
  const isDirectlyTestable = model.modelId.toLowerCase() !== 'auto'

  return (
    <div className="rounded-md border border-border/60 bg-secondary/30 px-3 py-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="break-words text-sm font-medium">
            {model.modelName || model.modelId}
          </div>
          {model.modelName && model.modelName !== model.modelId && (
            <div className="mt-0.5 break-all font-mono text-[11px] text-muted-foreground">
              {model.modelId}
            </div>
          )}
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          className="shrink-0"
          disabled={testDisabled || !isDirectlyTestable}
          onClick={onTest}
          title={isDirectlyTestable ? '发送真实模型请求' : '自动路由模型不可直接测试'}
        >
          {testing ? <Loader2 className="animate-spin" /> : <Play />}
          {testing ? '测试中' : isDirectlyTestable ? '真实测试' : '不可直测'}
        </Button>
      </div>

      {(model.maxInputTokens != null || model.maxOutputTokens != null) && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {model.maxInputTokens != null && (
            <Badge variant="secondary" className="tabular-nums">
              输入 {formatNumber(model.maxInputTokens)}
            </Badge>
          )}
          {model.maxOutputTokens != null && (
            <Badge variant="secondary" className="tabular-nums">
              输出 {formatNumber(model.maxOutputTokens)}
            </Badge>
          )}
        </div>
      )}

      {model.description && (
        <div className="mt-2 break-words text-xs leading-relaxed text-muted-foreground">
          {model.description}
        </div>
      )}

      {result && <TestResult result={result} />}
    </div>
  )
}

function TestResult({ result }: { result: ModelTestResult }) {
  if (result.status === 'error') {
    return (
      <div className="mt-3 flex items-start gap-2 border-t border-destructive/20 pt-3 text-xs text-destructive">
        <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
        <span className="min-w-0 break-words">{result.message}</span>
      </div>
    )
  }

  const { data } = result
  return (
    <div className="mt-3 space-y-2 border-t border-emerald-500/20 pt-3 text-xs">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-muted-foreground">
        <span className="flex items-center gap-1 font-medium text-emerald-600">
          <CheckCircle2 className="h-4 w-4" />请求成功
        </span>
        <span>凭据 #{data.credentialId}</span>
        <span className="flex items-center gap-1 tabular-nums">
          <Clock3 className="h-3.5 w-3.5" />{data.latencyMs} ms
        </span>
        {data.creditUsage != null && (
          <span className="tabular-nums">
            {formatCredit(data.creditUsage)} {data.creditUnit || 'credits'}
          </span>
        )}
      </div>
      <pre className="max-h-32 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-foreground">
        {data.responseText}
      </pre>
    </div>
  )
}

function formatCredit(value: number) {
  return value.toLocaleString('zh-CN', { maximumFractionDigits: 6 })
}
