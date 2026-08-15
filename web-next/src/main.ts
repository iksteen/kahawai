import { createApp } from 'vue'

import App from './App.vue'
import { authWire } from './api/auth-wire.ts'
import { scrubLegacyCredentials, startAuthSession } from './api/session.ts'
import './theme.css'

// Before anything can make a request: the transport asks the session for a
// bearer, and the session has to be listening for a peer tab's sign-out from
// the moment this tab exists rather than from its first fetch.
startAuthSession(authWire)
// An older build persisted credentials. This one writes none, and removes what
// it finds.
scrubLegacyCredentials()

createApp(App).mount('#app')
