import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './styles.css'
import { restoreCookie } from './api'
import App from './App'

// Before the first render: the media credential has to exist by the time the
// browser fetches the posters that render puts on the page.
restoreCookie()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
