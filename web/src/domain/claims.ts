/// Who the access token says you are.
///
/// For LABELLING only: the name in the header, and whether to offer the Admin
/// menu. Never an authorisation decision — the hub refuses every admin route
/// regardless of what this returns, and a client that trusted its own copy of
/// a claim would be trusting a string it was handed. What it buys is not
/// rendering a page of refusals to somebody who cannot use it.
///
/// A bearer is otherwise opaque here: the refresh clock comes from the
/// response's `expires_in` and not from `exp` — see `token.ts`.

export type Claims = { username: string; admin: boolean }

const NOBODY: Claims = { username: '', admin: false }

/// Base64URL, which is not base64.
///
/// `atob` on the raw segment is what the old client did, and it throws on the
/// `-` and `_` that base64url uses for 62 and 63 — which appear in most
/// payloads of any length. The throw was caught and turned into "no claims",
/// so an administrator intermittently did not get the Admin menu, and nobody
/// could see why.
///
/// The bytes are then decoded as UTF-8. `atob` returns a binary string, so
/// reading it directly turns a name like "Ingmár" into mojibake in the header.
/// No padding is restored: `atob` implements WHATWG forgiving-base64, which
/// accepts a segment with its `=` removed — as a JWT always has. Adding it
/// back was code no test could distinguish.
function payload(segment: string): unknown {
  const binary = atob(segment.replaceAll('-', '+').replaceAll('_', '/'))
  const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0))
  return JSON.parse(new TextDecoder().decode(bytes))
}

/// Never throws. A token this cannot read is somebody with no name and no
/// admin menu, which is the safe reading of an unreadable claim — and the
/// requests still carry the token, so the hub decides what it is worth.
export function claimsFrom(token: string | null | undefined): Claims {
  const segment = token?.split('.')[1]
  if (!segment) return NOBODY
  try {
    // No shape check on the payload itself. Destructuring a string or a number
    // yields undefined for both fields, and destructuring null throws into the
    // catch below — so every not-an-object case already lands on `NOBODY`, and
    // a guard for them was a line no test could tell apart.
    const { username, admin } = payload(segment) as { username?: unknown; admin?: unknown }
    return {
      username: typeof username === 'string' ? username : '',
      // Exactly `true`. A token carrying `"admin": "no"` is not an admin, and
      // any truthiness test says it is.
      admin: admin === true,
    }
  } catch {
    return NOBODY
  }
}
