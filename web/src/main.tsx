import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './styles.css'
import { scrubLegacyCredentials } from './api'
import App from './App'

// One-time protocol cutover: delete obsolete JavaScript-readable credentials
// without reading or migrating them.
scrubLegacyCredentials()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
