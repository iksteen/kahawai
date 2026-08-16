/// One query client, and the two defaults that are not TanStack's.
///
/// The retry rule is the app's, not a number: `retry` in `errors.ts` reads the
/// STATUS, which is the hub's published contract — 429 and 503 may work later,
/// 5xx is worth a backoff, every other 4xx is final. Retrying a 403 three
/// times is three requests to be told the same thing.
///
/// A 401 never reaches here: the transport refreshes and replays it once, and
/// a second 401 is an answer rather than a race.

import { QueryClient } from '@tanstack/vue-query'

import { retry } from './errors.ts'

/// Three attempts total. Past that a hub that is restarting is better reported
/// than waited on — every screen offers Try again, and that button is a person
/// who knows something the client does not.
const ATTEMPTS = 3

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: (failures, error) => failures + 1 < ATTEMPTS && retry(error),
        // Off, because the design is frozen and the old app did not do it.
        // Rows swapping under somebody who tabbed away and back is a change
        // worth making deliberately, on a screen where it helps, rather than
        // everywhere at once because it is the default.
        refetchOnWindowFocus: false,
      },
    },
  })
}
