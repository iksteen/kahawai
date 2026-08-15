/// The three auth calls the session drives, with the opt-outs they need.
///
/// Its own module so the session has no import of the generated bindings and
/// the bindings have none of the session — the transport sits between them and
/// is handed its token getter, rather than reaching for one.
///
/// Every call sets `skipAuthRefresh`. Without it a 401 from the refresh
/// endpoint re-enters `refreshTokens`, which returns the very promise it is
/// already inside: the refresh never settles and the session hangs until the
/// timeout fires. `login` and `refresh` also skip the Authorization header —
/// they are how a bearer is obtained, and sending a dead one invites the hub
/// to answer about that instead.

import type { AuthWire } from './session.ts'
import { login, logout, refresh } from './generated/kahawai.ts'

export const authWire: AuthWire = {
  login: (username, password) =>
    login(
      { client: 'browser', username, password },
      { skipAuthRefresh: true, skipAuthorization: true },
    ),
  refresh: () => refresh({ client: 'browser' }, { skipAuthRefresh: true, skipAuthorization: true }),
  logout: async (bearer) => {
    // The captured bearer explicitly, not whatever is in memory: by the time
    // this runs, memory has already been cleared.
    await logout(
      { client: 'browser' },
      { headers: { Authorization: `Bearer ${bearer}` }, skipAuthRefresh: true },
    )
  },
}
