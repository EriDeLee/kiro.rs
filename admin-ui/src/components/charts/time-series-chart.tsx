import { memo, useMemo } from 'react'
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { TimeSeriesPoint, StatsGranularity } from '@/types/api'
import { tooltipCursorStyle } from './tooltip-style'
import { formatCredits, formatNumber } from '@/lib/utils'

/**
 * 图表展示用的粒度。`week` 只存在于前端 —— 后端只有 hour / day 两种桶，周点是把天点
 * 按自然周合并出来的（见 overview-page 的 rollupWeekly），所以不进 StatsGranularity。
 */
export type ChartGranularity = StatsGranularity | 'week'

interface Props {
  data: TimeSeriesPoint[]
  granularity: ChartGranularity
}

const COLORS = {
  input: '#3b82f6',
  output: '#10b981',
  credits: '#ec4899',
} as const

/**
 * 输入挂左轴、输出挂右轴：两者量级差约 800 倍（实测周点 24 亿 vs 300 万），共用一个
 * 线性轴的话输出永远是一条压在 0 上的直线。代价是两条线的高低不再能直接比大小 ——
 * 图例与轴刻度同色标注，读的时候看轴。
 */
const SERIES = [
  { key: 'inputTokens', name: '输入', color: COLORS.input, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'outputTokens', name: '输出', color: COLORS.output, axis: 'right' as const, kind: 'tokens' as const },
]

interface ChartPoint extends TimeSeriesPoint {
  /** X 轴上的短标签 */
  label: string
  /** tooltip 里的长标签；周点在这里才展开成区间 */
  tooltipLabel: string
}

function monthDay(d: Date): string {
  return `${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
}

/**
 * X 轴短标签。周点用周首日期，格式与天点一致（带年份）—— 轴上不写区间：那个点本身就
 * 代表一周，标签再写一遍 `07-20~07-26` 是把同一件事说两遍，还会把年份挤掉。
 * 区间放 tooltip（见 formatTooltipTs）。
 */
function formatTs(ts: string, granularity: ChartGranularity): string {
  const d = new Date(ts)
  const md = monthDay(d)
  if (granularity === 'hour') return `${d.getFullYear()}-${md} ${String(d.getHours()).padStart(2, '0')}:00`
  return `${d.getFullYear()}-${md}`
}

/** tooltip 长标签：周点在这里展开成区间，免得与天点混淆 */
function formatTooltipTs(ts: string, granularity: ChartGranularity): string {
  if (granularity !== 'week') return formatTs(ts, granularity)
  const start = new Date(ts)
  const end = new Date(start)
  end.setDate(end.getDate() + 6)
  return `${start.getFullYear()}-${monthDay(start)} ~ ${monthDay(end)}`
}

function pickXAxisInterval(len: number): number | 'preserveStartEnd' {
  if (len <= 12) return 0
  if (len <= 48) return Math.ceil(len / 12)
  return Math.ceil(len / 16)
}

function ChartTooltip({ active, payload, label }: {
  active?: boolean
  payload?: ReadonlyArray<{
    dataKey?: string | number
    value?: number
    color?: string
    payload?: ChartPoint
  }>
  label?: string
}) {
  if (!active || !payload?.length) return null
  const map = new Map<string, number>()
  payload.forEach((p) => {
    if (typeof p.dataKey === 'string' && typeof p.value === 'number') {
      map.set(p.dataKey, p.value)
    }
  })
  const credits = payload[0]?.payload?.credits ?? 0
  const title = payload[0]?.payload?.tooltipLabel ?? label
  return (
    <div style={TOOLTIP_STYLE}>
      <div style={{ fontWeight: 600, marginBottom: 6, color: 'rgba(255,255,255,0.92)' }}>{title}</div>
      {SERIES.map((s) => <TooltipRow key={s.key} entry={s} value={map.get(s.key)} />)}
      {credits > 0 && <CreditTooltipRow credits={credits} />}
    </div>
  )
}

const TOOLTIP_STYLE: React.CSSProperties = {
  background: 'rgba(20,20,20,0.94)',
  border: '1px solid rgba(255,255,255,0.08)',
  borderRadius: 10,
  boxShadow: '0 8px 24px rgba(0,0,0,0.25)',
  color: '#fff',
  fontSize: 12,
  minWidth: 180,
  padding: '10px 14px',
}

const TOOLTIP_ROW_STYLE: React.CSSProperties = {
  alignItems: 'center',
  display: 'flex',
  gap: 8,
  padding: '2px 0',
}

const TOOLTIP_SWATCH_BASE_STYLE: React.CSSProperties = {
  borderRadius: 2,
  display: 'inline-block',
  height: 10,
  width: 10,
}

const TOOLTIP_VALUE_STYLE: React.CSSProperties = {
  fontVariantNumeric: 'tabular-nums',
}

function TooltipRow({
  entry,
  value,
}: {
  entry: (typeof SERIES)[number]
  value?: number
}) {
  if (value == null) return null
  const valueStr = formatNumber(value)
  return (
    <div style={TOOLTIP_ROW_STYLE}>
      <span style={{ ...TOOLTIP_SWATCH_BASE_STYLE, background: entry.color }} />
      <span style={{ flex: 1 }}>{entry.name}:</span>
      <span style={TOOLTIP_VALUE_STYLE}>{valueStr}</span>
    </div>
  )
}

function CreditTooltipRow({ credits }: { credits: number }) {
  return (
    <div style={CREDIT_ROW_STYLE}>
      <span style={{ ...TOOLTIP_SWATCH_BASE_STYLE, background: COLORS.credits }} />
      <span style={{ flex: 1 }}>Credit:</span>
      <span style={TOOLTIP_VALUE_STYLE}>{formatCredits(credits)}</span>
    </div>
  )
}

const CREDIT_ROW_STYLE: React.CSSProperties = {
  ...TOOLTIP_ROW_STYLE,
  borderTop: '1px solid rgba(255,255,255,0.08)',
  marginTop: 4,
  padding: '4px 0 0',
}

function TimeSeriesChartImpl({ data, granularity }: Props) {
  const formatted = useMemo<ChartPoint[]>(
    () =>
      data.map((p) => ({
        ...p,
        label: formatTs(p.ts, granularity),
        tooltipLabel: formatTooltipTs(p.ts, granularity),
      })),
    [data, granularity],
  )
  const interval = useMemo(() => pickXAxisInterval(formatted.length), [formatted.length])
  // 全零时强制让对应轴显示 0 刻度，避免空白
  const leftAllZero = useMemo(() => formatted.every((p) => p.inputTokens === 0), [formatted])
  const rightAllZero = useMemo(() => formatted.every((p) => p.outputTokens === 0), [formatted])

  return (
    <div className="h-[260px] sm:h-[320px]">
      <ResponsiveContainer width="100%" height="100%">
        {/* left 不能取负：Y 轴刻度文字是右对齐的，负边距会把最宽的那类标签（如 600M，
            比带小数点的 1.5B 宽）挤出容器左边缘裁掉一个字 */}
        <LineChart data={formatted} margin={{ top: 16, right: 0, left: 0, bottom: 0 }}>
          {chartAxes({ interval, leftAllZero, rightAllZero })}
          <Tooltip content={<ChartTooltip />} cursor={tooltipCursorStyle} />
          {chartLegend()}
          {chartLines()}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

interface AxisTickProps {
  index?: number
  payload?: { value?: string | number }
  visibleTicksCount?: number
  x?: number
  y?: number
}

/**
 * X 轴刻度：首个标签左对齐、末个标签右对齐，中间居中。
 *
 * recharts 默认把每个标签居中在它的数据点上，最左最右那两个于是各有一半落在绘图区外
 * 被容器裁掉（实测末尾的 `2026-08-15` 显示成 `2026-0`）。靠加大 margin 只能按最宽的
 * 标签去猜一个数字，换个粒度就又不够；改锚点是与标签宽度无关的解法。
 */
function AxisTick({ index = 0, payload, visibleTicksCount = 0, x = 0, y = 0 }: AxisTickProps) {
  const anchor = index === 0 ? 'start' : index === visibleTicksCount - 1 ? 'end' : 'middle'
  return (
    <text x={x} y={y} dy={12} textAnchor={anchor} fontSize={11} className="fill-muted-foreground">
      {String(payload?.value ?? '')}
    </text>
  )
}

function chartAxes({
  interval,
  leftAllZero,
  rightAllZero,
}: {
  interval: number | 'preserveStartEnd'
  leftAllZero: boolean
  rightAllZero: boolean
}) {
  return [
    <CartesianGrid key="grid" strokeDasharray="3 3" className="stroke-border/50" />,
    <XAxis
      key="x"
      dataKey="label"
      tick={<AxisTick />}
      className="fill-muted-foreground"
      interval={interval}
    />,
    <YAxis
      key="left"
      yAxisId="left"
      tick={{ fill: COLORS.input, fontSize: 11 }}
      tickFormatter={(v: number) => formatNumber(v)}
      width={48}
      domain={leftAllZero ? [0, 1] : [0, 'auto']}
      ticks={leftAllZero ? [0] : undefined}
      allowDecimals={false}
    />,
    <YAxis
      key="right"
      yAxisId="right"
      orientation="right"
      tick={{ fill: COLORS.output, fontSize: 11 }}
      tickFormatter={(v: number) => formatNumber(v)}
      width={48}
      domain={rightAllZero ? [0, 1] : [0, 'auto']}
      ticks={rightAllZero ? [0] : undefined}
      allowDecimals={false}
    />,
  ]
}

function chartLegend() {
  return <Legend verticalAlign="top" align="center" iconType="circle" wrapperStyle={LEGEND_STYLE} />
}

const LEGEND_STYLE: React.CSSProperties = {
  fontSize: 12,
  paddingBottom: 8,
}

function chartLines() {
  return SERIES.map((s) => (
    <Line
      key={s.key}
      yAxisId={s.axis}
      type="monotone"
      dataKey={s.key}
      stroke={s.color}
      name={s.name}
      dot={false}
      strokeWidth={2}
      isAnimationActive
      animationDuration={550}
      animationEasing="ease-out"
    />
  ))
}

export const TimeSeriesChart = memo(TimeSeriesChartImpl)
