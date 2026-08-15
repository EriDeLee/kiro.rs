import { useMemo, useState } from 'react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Activity, Calendar, Coins, Cpu, KeyRound, Server } from 'lucide-react'
import { useByCredential, useByModel, useOverview, useTimeSeries } from '@/hooks/use-stats'
import { useClientKeys } from '@/hooks/use-client-keys'
import { useGroupOptions } from '@/hooks/use-groups'
import type {
  ClientKeyItem,
  CredentialDistribution,
  ModelDistribution,
  StatsFilter,
  StatsGranularity,
  StatsRange,
  StatsTimeFilter,
  TimeSeriesPoint,
} from '@/types/api'
import { TimeSeriesChart } from '@/components/charts/time-series-chart'
import { ModelPieChart } from '@/components/charts/model-pie-chart'
import { CredentialBarChart } from '@/components/charts/credential-bar-chart'
import { cn, formatCredits, formatNumber } from '@/lib/utils'
import type { ChartGranularity } from '@/components/charts/time-series-chart'
import { Input } from '@/components/ui/input'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

const RANGES: { label: string; value: StatsRange }[] = [
  { label: '24 小时', value: '24h' },
  { label: '7 天', value: '7d' },
  { label: '30 天', value: '30d' },
  { label: '12 个月', value: '12m' },
]

/** 「12 个月」视图的天数跨度；图表按 7 天一组合并成周点，约 52 个 */
const LONG_RANGE_DAYS = 364
/**
 * 后端天桶容量，与 `usage_stats.rs` 的 `STATS_WINDOW_DAYS` 对齐。
 * 自定义区间超过这个跨度时，更早的那部分没有桶，只用于提示而不改查询。
 */
const STATS_WINDOW_DAYS = 400

const GRANULARITY_LABELS: Record<ChartGranularity, string> = {
  hour: '按小时',
  day: '按天',
  week: '按周',
}

/**
 * 粒度由区间唯一决定，不给选。
 *
 * - `24h`：小时。这也是唯一用小时的地方 —— 后端小时桶只有 31 天
 *   （`HOUR_BUCKETS = 24 * 31`），更长的区间按小时查会只返回最近 31 天的点，
 *   卡片总额跟着变小且不报错，所以别把小时放开给别的区间。
 * - `7d` / `30d` / 日历自定义（`undefined`）：天。
 * - `12m`：周。一年 365 个天点挤成一片，合并成 52 个周点才看得清。
 */
function granularityForRange(range?: StatsRange): ChartGranularity {
  if (range === '24h') return 'hour'
  if (range === '12m') return 'week'
  return 'day'
}

/** 周点是前端合并出来的，发给后端的查询粒度仍是天 */
function queryGranularityOf(display: ChartGranularity): StatsGranularity {
  return display === 'week' ? 'day' : display
}

function toDateInputValue(d: Date): string {
  const year = d.getFullYear()
  const month = String(d.getMonth() + 1).padStart(2, '0')
  const day = String(d.getDate()).padStart(2, '0')
  return `${year}-${month}-${day}`
}

function customTimeFilter(
  startDate: string,
  endDate: string,
  granularity: StatsGranularity,
): StatsTimeFilter {
  return { startDate, endDate, granularity }
}

function presetStartDate(range: StatsRange, endDate: string): string {
  const days =
    range === '24h' ? 1 : range === '7d' ? 6 : range === '30d' ? 29 : LONG_RANGE_DAYS
  const d = new Date(`${endDate}T00:00:00`)
  d.setDate(d.getDate() - days)
  return toDateInputValue(d)
}

/** 该时间点所在自然周的周一 0 点（本地时区，与后端天桶同一时区口径） */
function weekStartOf(ts: string): Date {
  const d = new Date(ts)
  const monday = new Date(d.getFullYear(), d.getMonth(), d.getDate())
  monday.setDate(monday.getDate() - ((monday.getDay() + 6) % 7))
  return monday
}

function emptyWeek(ts: string): TimeSeriesPoint {
  return { ts, inputTokens: 0, outputTokens: 0, calls: 0, errors: 0, credits: 0 }
}

/**
 * 把天点合并成**自然周**点。仅用于图表展示；卡片总额走 aggregateSeries 对原始天点
 * 求和，不受这里影响（两者求和结果相同）。
 *
 * 必须按日历分箱、不能按"每 7 个点一组"：后端只返回有数据的天，没请求的那些天根本
 * 没有桶。按点数切的话，一段空档会把它两侧的日子挤进同一组 —— 实测出现过一个"周"
 * 横跨 7/27 到 8/8 共 13 个日历天，X 轴就不再是时间轴了。
 *
 * 同理，首尾之间缺的周补 0，让空档显示成真实的低谷而不是被折叠掉。首周之前不补，
 * 免得「12 个月」视图拖着一长条无数据的零线。
 */
function rollupWeekly(data: TimeSeriesPoint[]): TimeSeriesPoint[] {
  if (data.length === 0) return data
  const buckets = new Map<number, TimeSeriesPoint>()
  for (const p of data) {
    const start = weekStartOf(p.ts)
    const key = start.getTime()
    const cur = buckets.get(key)
    if (cur) {
      cur.inputTokens += p.inputTokens
      cur.outputTokens += p.outputTokens
      cur.calls += p.calls
      cur.errors += p.errors
      cur.credits += p.credits ?? 0
    } else {
      buckets.set(key, {
        ts: start.toISOString(),
        inputTokens: p.inputTokens,
        outputTokens: p.outputTokens,
        calls: p.calls,
        errors: p.errors,
        credits: p.credits ?? 0,
      })
    }
  }
  const keys = [...buckets.keys()].sort((a, b) => a - b)
  const last = keys[keys.length - 1]
  const out: TimeSeriesPoint[] = []
  // 用 setDate 而不是加固定毫秒数推进，跨夏令时也能落在本地周一 0 点
  for (const cursor = new Date(keys[0]); cursor.getTime() <= last; cursor.setDate(cursor.getDate() + 7)) {
    out.push(buckets.get(cursor.getTime()) ?? emptyWeek(cursor.toISOString()))
  }
  return out
}

/** 含首尾的天数跨度；任一端为空返回 0 */
function daysBetween(startDate?: string, endDate?: string): number {
  if (!startDate || !endDate) return 0
  const start = new Date(`${startDate}T00:00:00`).getTime()
  const end = new Date(`${endDate}T00:00:00`).getTime()
  if (Number.isNaN(start) || Number.isNaN(end)) return 0
  return Math.round((end - start) / 86400000) + 1
}

function formatDateText(value: string): string {
  return value.replace(/-/g, '/')
}

function timeLabel(filter: StatsTimeFilter, display: ChartGranularity): string {
  const suffix = GRANULARITY_LABELS[display]
  if (filter.range) {
    const range = RANGES.find((r) => r.value === filter.range)?.label ?? filter.range
    return `近 ${range} · ${suffix}`
  }
  return `${formatDateText(filter.startDate ?? '')} - ${formatDateText(filter.endDate ?? '')} · ${suffix}`
}

export function OverviewPage() {
  const filters = useOverviewFilters()
  const { data: overview } = useOverview()
  const { data: keysData } = useClientKeys()
  const groupOptions = useGroupOptions()
  const { data: series } = useTimeSeries(filters.timeFilter, filters.statsFilter)
  const { data: byModel } = useByModel(filters.timeFilter, filters.statsFilter)
  const { data: byCred } = useByCredential(filters.timeFilter, filters.statsFilter)
  const seriesData = useMemo(() => series ?? [], [series])
  const modelData = useMemo(() => byModel ?? [], [byModel])
  const credData = useMemo(() => byCred ?? [], [byCred])
  const rangeStats = useMemo(() => aggregateSeries(seriesData), [seriesData])
  // 「按周」时把天点合并成周点再画，否则一年 365 根线挤成一片。
  // 卡片用的是未合并的 seriesData，两者求和相同。
  const chartData = useMemo(
    () => (filters.appliedGranularity === 'week' ? rollupWeekly(seriesData) : seriesData),
    [seriesData, filters.appliedGranularity],
  )
  const selectedKeyLabel = selectedStatsKeyLabel(filters.keyFilter, keysData?.keys ?? [])
  const groupFilterActive = filters.groupFilter !== 'all'

  return (
    <div>
      <PageHeader />
      <StatsCards
        activeCredentials={overview?.activeCredentials ?? 0}
        activeKeys={overview?.activeClientKeys ?? 0}
        stats={rangeStats}
        timeText={timeLabel(filters.timeFilter, filters.appliedGranularity)}
      />
      <KeyFilterCard
        keyFilter={filters.keyFilter}
        keys={keysData?.keys ?? []}
        selectedLabel={selectedKeyLabel}
        onChange={filters.setKeyFilter}
        groupFilter={filters.groupFilter}
        groupOptions={groupOptions}
        onGroupChange={filters.setGroupFilter}
      />
      <TrendCard
        appliedGranularity={filters.appliedGranularity}
        customEndDate={filters.customEndDate}
        customStartDate={filters.customStartDate}
        draftRange={filters.draftRange}
        keyFilter={filters.keyFilter}
        seriesData={chartData}
        timeFilter={filters.timeFilter}
        onCustomEndDateChange={filters.setCustomEndDate}
        onCustomStartDateChange={filters.setCustomStartDate}
        onPresetRangeChange={filters.selectPresetRange}
        rangeInvalid={filters.rangeInvalid}
        spanExceedsWindow={filters.spanExceedsWindow}
      />
      <DistributionPanels
        byCred={credData}
        byModel={modelData}
        timeText={timeLabel(filters.timeFilter, filters.appliedGranularity)}
        groupFilterActive={groupFilterActive}
      />
    </div>
  )
}

function useOverviewFilters() {
  const today = useMemo(() => toDateInputValue(new Date()), [])
  const initialStart = useMemo(() => presetStartDate('24h', today), [today])
  const [timeFilter, setTimeFilter] = useState<StatsTimeFilter>(() =>
    customTimeFilter(initialStart, today, 'hour'),
  )
  const [customStartDate, setCustomStartDate] = useState(initialStart)
  const [customEndDate, setCustomEndDate] = useState(today)
  // 已应用的展示粒度。图表按它决定是否合并成周。它必须是 state 而不是从当前区间派生：
  // 日期填成非法时不发查询、图仍是上一次的结果，派生值会先变，标题就与图对不上。
  const [appliedGranularity, setAppliedGranularity] = useState<ChartGranularity>('hour')
  const [draftRange, setDraftRange] = useState<StatsRange | undefined>('24h')
  const [keyFilter, setKeyFilter] = useState('all')
  const [groupFilter, setGroupFilter] = useState('all')
  const statsFilter = useMemo<StatsFilter>(() => {
    const f: StatsFilter = {}
    if (keyFilter !== 'all') f.keyId = Number(keyFilter)
    if (groupFilter !== 'all') f.group = groupFilter
    return f
  }, [keyFilter, groupFilter])
  const rangeInvalid = !customStartDate || !customEndDate || customEndDate < customStartDate
  const spanExceedsWindow = daysBetween(customStartDate, customEndDate) > STATS_WINDOW_DAYS

  /**
   * 立即生效。日期非法（缺一端 / 结束早于开始）时不发查询，保留上一次的有效结果 ——
   * 界面上另有一行提示说明为什么没变，不做无声的空操作。
   */
  const apply = (startDate: string, endDate: string, range?: StatsRange) => {
    if (!startDate || !endDate || endDate < startDate) return
    const display = granularityForRange(range)
    setTimeFilter(customTimeFilter(startDate, endDate, queryGranularityOf(display)))
    setAppliedGranularity(display)
  }

  const updateCustomStartDate = (value: string) => {
    setCustomStartDate(value)
    setDraftRange(undefined)
    apply(value, customEndDate)
  }
  const updateCustomEndDate = (value: string) => {
    setCustomEndDate(value)
    setDraftRange(undefined)
    apply(customStartDate, value)
  }
  const selectPresetRange = (range: StatsRange) => {
    const endDate = toDateInputValue(new Date())
    const startDate = presetStartDate(range, endDate)
    setCustomStartDate(startDate)
    setCustomEndDate(endDate)
    setDraftRange(range)
    apply(startDate, endDate, range)
  }
  return {
    appliedGranularity,
    customEndDate,
    customStartDate,
    draftRange,
    keyFilter,
    groupFilter,
    rangeInvalid,
    selectPresetRange,
    setCustomEndDate: updateCustomEndDate,
    setCustomStartDate: updateCustomStartDate,
    setKeyFilter,
    spanExceedsWindow,
    setGroupFilter,
    statsFilter,
    timeFilter,
  }
}

function selectedStatsKeyLabel(keyFilter: string, keys: ClientKeyItem[]): string {
  if (keyFilter === 'all') return '全部入口 Key'
  return keys.find((k) => String(k.id) === keyFilter)?.name ?? `#${keyFilter}`
}

function PageHeader() {
  return (
    <div className="mb-6">
      <h1 className="text-[28px] font-semibold tracking-tight leading-tight">概览</h1>
      <p className="mt-1 text-sm text-muted-foreground">
        中转站调用情况、Token 消耗趋势与上游凭据贡献
      </p>
    </div>
  )
}

interface RangeStats {
  calls: number
  credits: number
  errors: number
  inputTokens: number
  outputTokens: number
}

function aggregateSeries(data: TimeSeriesPoint[]): RangeStats {
  return data.reduce(
    (acc, p) => ({
      calls: acc.calls + p.calls,
      credits: acc.credits + (p.credits ?? 0),
      errors: acc.errors + p.errors,
      inputTokens: acc.inputTokens + p.inputTokens,
      outputTokens: acc.outputTokens + p.outputTokens,
    }),
    { calls: 0, credits: 0, errors: 0, inputTokens: 0, outputTokens: 0 },
  )
}

function StatsCards({
  activeCredentials,
  activeKeys,
  stats,
  timeText,
}: {
  activeCredentials: number
  activeKeys: number
  stats: RangeStats
  timeText: string
}) {
  const cards = [
    {
      icon: <Activity className="h-4 w-4" />,
      label: '调用',
      value: formatNumber(stats.calls),
      extra: stats.errors > 0 ? (
        <Badge variant="destructive">异常 {formatNumber(stats.errors)}</Badge>
      ) : null,
    },
    { icon: <Cpu className="h-4 w-4" />, label: '输入 Token', value: formatNumber(stats.inputTokens) },
    { icon: <Cpu className="h-4 w-4" />, label: '输出 Token', value: formatNumber(stats.outputTokens) },
    {
      icon: <Coins className="h-4 w-4" />,
      label: 'Credit',
      value: formatCredits(stats.credits),
      extra: <span className="text-[11px] text-muted-foreground">上游计费量</span>,
    },
    {
      icon: <KeyRound className="h-4 w-4" />,
      label: '启用的客户端 Key',
      meta: '当前可用入口',
      value: formatNumber(activeKeys),
      className: 'col-span-2 max-[360px]:col-span-1 lg:col-span-1',
      extra: (
        <span className="text-[11px] text-muted-foreground">
          上游 {formatNumber(activeCredentials)}
        </span>
      ),
    },
  ]

  return (
    <div className="mb-6 grid grid-cols-2 gap-3 max-[360px]:grid-cols-1 lg:grid-cols-5">
      {cards.map((card) => (
        <StatCard key={card.label} meta={card.meta ?? timeText} {...card} />
      ))}
    </div>
  )
}

function KeyFilterCard({
  keyFilter,
  keys,
  onChange,
  selectedLabel,
  groupFilter,
  groupOptions,
  onGroupChange,
}: {
  keyFilter: string
  keys: ClientKeyItem[]
  onChange: (value: string) => void
  selectedLabel: string
  groupFilter: string
  groupOptions: string[]
  onGroupChange: (value: string) => void
}) {
  return (
    <Card className="mb-6">
      <CardContent className="p-4 sm:p-5">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div className="min-w-0 flex-1">
            <div className="text-sm font-medium">统计筛选</div>
            <div className="truncate text-[12px] text-muted-foreground">
              {selectedLabel}
              {groupFilter !== 'all' && ` · 分组：${groupFilter}`}
            </div>
          </div>
          <div className="flex flex-col gap-2 sm:flex-row">
            {/* 入口 Key 筛选 */}
            <Select value={keyFilter} onValueChange={onChange}>
              <SelectTrigger className="h-8 w-full sm:w-[180px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="end">
                <SelectItem value="all">全部入口 Key</SelectItem>
                {keys.map((key) => (
                  <SelectItem key={key.id} value={String(key.id)}>
                    {key.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {/* 账号分组筛选 */}
            <Select value={groupFilter} onValueChange={onGroupChange}>
              <SelectTrigger className="h-8 w-full sm:w-[180px]">
                <SelectValue placeholder="全部分组" />
              </SelectTrigger>
              <SelectContent align="end">
                <SelectItem value="all">全部分组</SelectItem>
                {groupOptions.map((g) => (
                  <SelectItem key={g} value={g}>
                    {g}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

interface TrendCardProps {
  appliedGranularity: ChartGranularity
  customEndDate: string
  customStartDate: string
  draftRange?: StatsRange
  keyFilter: string
  onCustomEndDateChange: (value: string) => void
  onCustomStartDateChange: (value: string) => void
  onPresetRangeChange: (value: StatsRange) => void
  rangeInvalid: boolean
  seriesData: TimeSeriesPoint[]
  spanExceedsWindow: boolean
  timeFilter: StatsTimeFilter
}

function TrendCard({
  appliedGranularity,
  customEndDate,
  customStartDate,
  draftRange,
  keyFilter,
  onCustomEndDateChange,
  onCustomStartDateChange,
  onPresetRangeChange,
  rangeInvalid,
  seriesData,
  spanExceedsWindow,
  timeFilter,
}: TrendCardProps) {
  const chartKey = `${timeLabel(timeFilter, appliedGranularity)}:${keyFilter}`
  return (
    <Card className="mb-6">
      <CardContent className="p-4 sm:p-5">
        <div className="mb-4 flex flex-col gap-3 lg:flex-row lg:items-start lg:justify-between">
          <TrendTitle granularity={appliedGranularity} />
          <TrendControls
            customEndDate={customEndDate}
            customStartDate={customStartDate}
            draftRange={draftRange}
            onCustomEndDateChange={onCustomEndDateChange}
            onCustomStartDateChange={onCustomStartDateChange}
            onPresetRangeChange={onPresetRangeChange}
            rangeInvalid={rangeInvalid}
            spanExceedsWindow={spanExceedsWindow}
          />
        </div>
        <div key={chartKey} className="chart-range-fade">
          <TimeSeriesChart data={seriesData} granularity={appliedGranularity} />
        </div>
      </CardContent>
    </Card>
  )
}

function TrendTitle({ granularity }: { granularity: ChartGranularity }) {
  return (
    <div>
      <h2 className="text-base font-semibold tracking-tight">Token 使用趋势</h2>
      <p className="text-[12px] text-muted-foreground">
        {GRANULARITY_LABELS[granularity]}聚合 · 输入/输出
      </p>
    </div>
  )
}

function TrendControls({
  customEndDate,
  customStartDate,
  draftRange,
  onCustomEndDateChange,
  onCustomStartDateChange,
  onPresetRangeChange,
  rangeInvalid,
  spanExceedsWindow,
}: {
  customEndDate: string
  customStartDate: string
  draftRange?: StatsRange
  onCustomEndDateChange: (value: string) => void
  onCustomStartDateChange: (value: string) => void
  onPresetRangeChange: (value: StatsRange) => void
  rangeInvalid: boolean
  spanExceedsWindow: boolean
}) {
  return (
    <div className="flex w-full flex-col gap-2 lg:w-auto lg:flex-row lg:flex-wrap lg:items-end lg:justify-end">
      <PresetRangeButtons currentRange={draftRange} onChange={onPresetRangeChange} />
      <DateRangeInputs
        endDate={customEndDate}
        invalid={rangeInvalid}
        spanExceedsWindow={spanExceedsWindow}
        startDate={customStartDate}
        onEndDateChange={onCustomEndDateChange}
        onStartDateChange={onCustomStartDateChange}
      />
    </div>
  )
}

function PresetRangeButtons({
  currentRange,
  onChange,
}: {
  currentRange?: StatsRange
  onChange: (value: StatsRange) => void
}) {
  return (
    <div className="grid grid-cols-2 gap-1 rounded-md border border-border/60 p-0.5 lg:flex lg:items-center">
      {RANGES.map((r) => (
        <Button
          key={r.value}
          size="sm"
          variant={currentRange === r.value ? 'default' : 'ghost'}
          className="h-8 rounded-md px-2 text-xs lg:h-7 lg:px-3"
          onClick={() => onChange(r.value)}
        >
          {r.label}
        </Button>
      ))}
    </div>
  )
}

function DateRangeInputs({
  endDate,
  invalid,
  onEndDateChange,
  onStartDateChange,
  spanExceedsWindow,
  startDate,
}: {
  endDate: string
  invalid: boolean
  onEndDateChange: (value: string) => void
  onStartDateChange: (value: string) => void
  spanExceedsWindow: boolean
  startDate: string
}) {
  return (
    <div className="min-w-0">
      <div className="grid grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-2 max-[374px]:grid-cols-1 lg:flex lg:items-center">
        <DateInput value={startDate} onChange={onStartDateChange} />
        <span className="text-center text-xs text-muted-foreground max-[374px]:hidden">至</span>
        <DateInput value={endDate} onChange={onEndDateChange} />
      </div>
      {/* 日期非法时上面的图保持上一次的结果，这里说明原因，不做无声的空操作 */}
      {invalid && (
        <p className="mt-1 text-[11px] text-destructive">
          结束日期不能早于开始日期，图表仍显示上一次的区间
        </p>
      )}
      {/* 超出统计窗口的那一段没有桶，查了也是空的，说清楚而不是让人以为那段真的没用量 */}
      {!invalid && spanExceedsWindow && (
        <p className="mt-1 text-[11px] text-muted-foreground">
          统计只保留最近 {STATS_WINDOW_DAYS} 天，更早的区间没有数据
        </p>
      )}
    </div>
  )
}

function DateInput({ onChange, value }: { onChange: (value: string) => void; value: string }) {
  return (
    <div className="relative min-w-0">
      <Calendar className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
      <Input
        type="date"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="h-8 min-w-0 w-full rounded-md pl-8 text-xs lg:w-[145px]"
      />
    </div>
  )
}

function DistributionPanels({
  byCred,
  byModel,
  timeText,
  groupFilterActive,
}: {
  byCred: CredentialDistribution[]
  byModel: ModelDistribution[]
  timeText: string
  groupFilterActive: boolean
}) {
  return (
    <div className="mb-6 grid gap-4 lg:grid-cols-2">
      <ModelPanel data={byModel} timeText={timeText} groupFilterActive={groupFilterActive} />
      <CredentialPanel data={byCred} />
    </div>
  )
}

function ModelPanel({
  data,
  timeText,
  groupFilterActive,
}: {
  data: ModelDistribution[]
  timeText: string
  groupFilterActive: boolean
}) {
  return (
    <Card>
      <CardContent className="p-4 sm:p-5">
        <div className="mb-3 flex flex-col gap-1 sm:flex-row sm:items-center sm:justify-between">
          <h2 className="text-base font-semibold tracking-tight">按模型分布</h2>
          <span className="text-[11px] text-muted-foreground">{timeText}</span>
        </div>
        {groupFilterActive && (
          <p className="mb-3 rounded-md bg-amber-500/10 px-2.5 py-1.5 text-[11px] text-amber-600">
            当前已启用「分组筛选」。模型分布暂未细分到分组维度，本卡片显示的是
            <strong className="mx-0.5">不区分分组</strong>的模型聚合结果。
          </p>
        )}
        <ModelPieChart data={data} />
        {data.length > 0 && <ModelTable data={data} />}
      </CardContent>
    </Card>
  )
}

function ModelTable({ data }: { data: ModelDistribution[] }) {
  return (
    <div className="mt-3 max-h-32 overflow-auto text-[12px]">
      <table className="min-w-[420px] w-full">
        <thead className="text-muted-foreground">
          <tr>
            <th className="text-left font-medium pb-1">模型</th>
            <th className="text-right font-medium">调用</th>
            <th className="text-right font-medium">输入</th>
            <th className="text-right font-medium">输出</th>
          </tr>
        </thead>
        <tbody>
          {data.map((m) => (
            <tr key={m.model} className="border-t border-border/40">
              <td className="py-1 truncate">{m.model}</td>
              <td className="text-right tabular-nums">{formatNumber(m.calls)}</td>
              <td className="text-right tabular-nums">{formatNumber(m.inputTokens)}</td>
              <td className="text-right tabular-nums">{formatNumber(m.outputTokens)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function CredentialPanel({ data }: { data: CredentialDistribution[] }) {
  return (
    <Card>
      <CardContent className="p-4 sm:p-5">
        <div className="mb-3 flex items-center justify-between gap-3">
          <h2 className="text-base font-semibold tracking-tight">按上游凭据分布</h2>
          <span className="text-[11px] text-muted-foreground inline-flex items-center gap-1">
            <Server className="h-3 w-3" />Top {Math.min(data.length, 12)}
          </span>
        </div>
        <CredentialBarChart data={data} />
      </CardContent>
    </Card>
  )
}

function StatCard({
  icon,
  label,
  meta,
  value,
  extra,
  className,
}: {
  className?: string
  icon: React.ReactNode
  label: string
  meta: string
  value: string
  extra?: React.ReactNode
}) {
  return (
    <Card className={cn('hover:shadow-apple-lg hover:-translate-y-0.5', className)}>
      <CardContent className="p-4 sm:p-5">
        <div className="flex min-h-[34px] items-start gap-2">
          <div className="mt-0.5 shrink-0 text-muted-foreground">{icon}</div>
          <div className="min-w-0">
            <div className="truncate text-[13px] font-medium text-foreground">{label}</div>
            <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{meta}</div>
          </div>
        </div>
        <div className="ml-6 mt-4 flex min-h-[36px] items-end justify-between gap-3">
          <span className="min-w-0 truncate text-2xl font-semibold tracking-tight tabular-nums sm:text-3xl">{value}</span>
          <div className="shrink-0">{extra}</div>
        </div>
      </CardContent>
    </Card>
  )
}
