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

interface Props {
  data: TimeSeriesPoint[]
  granularity: StatsGranularity
}

const COLORS = {
  input: '#3b82f6',
  output: '#10b981',
  credits: '#ec4899',
} as const

const SERIES = [
  { key: 'inputTokens', name: '输入', color: COLORS.input, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'outputTokens', name: '输出', color: COLORS.output, axis: 'left' as const, kind: 'tokens' as const },
]

interface ChartPoint extends TimeSeriesPoint {
  label: string
}

function formatTs(ts: string, granularity: StatsGranularity): string {
  const d = new Date(ts)
  const md = `${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
  if (granularity === 'day') return `${d.getFullYear()}-${md}`
  return `${d.getFullYear()}-${md} ${String(d.getHours()).padStart(2, '0')}:00`
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
  return (
    <div style={TOOLTIP_STYLE}>
      <div style={{ fontWeight: 600, marginBottom: 6, color: 'rgba(255,255,255,0.92)' }}>{label}</div>
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
      })),
    [data, granularity],
  )
  const interval = useMemo(() => pickXAxisInterval(formatted.length), [formatted.length])
  // 全零时强制让左轴显示 0 刻度，避免空白
  const leftAllZero = useMemo(
    () =>
      formatted.every(
        (p) =>
          p.inputTokens === 0 && p.outputTokens === 0,
      ),
    [formatted],
  )

  return (
    <div className="h-[260px] sm:h-[320px]">
      <ResponsiveContainer width="100%" height="100%">
        <LineChart data={formatted} margin={{ top: 16, right: 6, left: -12, bottom: 0 }}>
          {chartAxes({ interval, leftAllZero })}
          <Tooltip content={<ChartTooltip />} cursor={tooltipCursorStyle} />
          {chartLegend()}
          {chartLines()}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

function chartAxes({
  interval,
  leftAllZero,
}: {
  interval: number | 'preserveStartEnd'
  leftAllZero: boolean
}) {
  return [
    <CartesianGrid key="grid" strokeDasharray="3 3" className="stroke-border/50" />,
    <XAxis
      key="x"
      dataKey="label"
      tick={{ fontSize: 11 }}
      className="fill-muted-foreground"
      interval={interval}
    />,
    <YAxis
      key="left"
      yAxisId="left"
      tick={{ fontSize: 11 }}
      className="fill-muted-foreground"
      tickFormatter={(v: number) => formatNumber(v)}
      width={48}
      domain={leftAllZero ? [0, 1] : [0, 'auto']}
      ticks={leftAllZero ? [0] : undefined}
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
