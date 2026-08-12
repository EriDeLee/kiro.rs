import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import './index.css'

const systemTheme = window.matchMedia('(prefers-color-scheme: dark)')
const applySystemTheme = ({ matches }: Pick<MediaQueryList, 'matches'>) => {
  document.documentElement.classList.toggle('dark', matches)
  document.documentElement.style.colorScheme = matches ? 'dark' : 'light'
}

applySystemTheme(systemTheme)
systemTheme.addEventListener('change', applySystemTheme)

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5000,
      refetchOnWindowFocus: false,
    },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
)
