const API_KEY_STORAGE_KEY = 'adminApiKey'
const CREDENTIAL_VIEW_KEY = 'credentialView'
const CREDENTIAL_PAGE_SIZE_KEY = 'credentialPageSize'
const CREDENTIAL_SORT_FIELD_KEY = 'credentialSortField'
const CREDENTIAL_SORT_DIR_KEY = 'credentialSortDir'

export type CredentialView = 'card' | 'list'

/** 排序字段：'manual' = 服务端顺序（保留拖拽调优先级） */
export type CredentialSortField =
  | 'manual'
  | 'priority'
  | 'successCount'
  | 'totalFailureCount'
  | 'lastUsedAt'
  | 'id'
export type CredentialSortDir = 'asc' | 'desc'

/** 每页数量：0 视为“全部”（不分页） */
const DEFAULT_PAGE_SIZE = 12

// 读回时按白名单校验：手改过 / 版本更迭遗留的值一律回落到默认，
// 否则会带着一个排序分支不认识的字段进入比较器，表现为排序静默失效。
const SORT_FIELDS: readonly CredentialSortField[] = [
  'manual',
  'priority',
  'successCount',
  'totalFailureCount',
  'lastUsedAt',
  'id',
]

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  // 凭据列表的展示形态（卡片 / 列表），默认卡片
  getCredentialView: (): CredentialView =>
    localStorage.getItem(CREDENTIAL_VIEW_KEY) === 'list' ? 'list' : 'card',
  setCredentialView: (view: CredentialView) =>
    localStorage.setItem(CREDENTIAL_VIEW_KEY, view),

  // 凭据列表每页数量（0 = 全部），默认 12
  getCredentialPageSize: (): number => {
    const raw = localStorage.getItem(CREDENTIAL_PAGE_SIZE_KEY)
    if (raw === null) return DEFAULT_PAGE_SIZE
    const n = Number(raw)
    return Number.isInteger(n) && n >= 0 ? n : DEFAULT_PAGE_SIZE
  },
  setCredentialPageSize: (size: number) =>
    localStorage.setItem(CREDENTIAL_PAGE_SIZE_KEY, String(size)),

  // 凭据列表的排序字段与方向，默认“手动顺序 / 升序”
  getCredentialSortField: (): CredentialSortField => {
    const raw = localStorage.getItem(CREDENTIAL_SORT_FIELD_KEY)
    return SORT_FIELDS.includes(raw as CredentialSortField)
      ? (raw as CredentialSortField)
      : 'manual'
  },
  getCredentialSortDir: (): CredentialSortDir =>
    localStorage.getItem(CREDENTIAL_SORT_DIR_KEY) === 'desc' ? 'desc' : 'asc',
  setCredentialSort: (field: CredentialSortField, dir: CredentialSortDir) => {
    localStorage.setItem(CREDENTIAL_SORT_FIELD_KEY, field)
    localStorage.setItem(CREDENTIAL_SORT_DIR_KEY, dir)
  },
}
