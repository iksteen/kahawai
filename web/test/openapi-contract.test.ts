/// The refusal contract, checked against the published document.
///
/// `docs/kahawai-implementation.md` states this in prose and the hub's
/// `error.rs` states it in a module doc, and prose is exactly what went stale:
/// four separate review rounds found the document contradicting the sentence
/// that described it — routes documenting a 415 they cannot return, routes
/// returning a 400 they never declared, an error body that was still
/// `text/plain`. Each was found by hand and would have been found here.
///
/// This reads `openapi.json`, which is generated from the hub and gated by the
/// fingerprint, so it fails on the change rather than on the next review.

import { strict as assert } from 'node:assert'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

type Response = {
  description?: string
  content?: Record<string, { schema?: { $ref?: string } }>
}
type Operation = {
  summary?: string
  description?: string
  parameters?: unknown[]
  requestBody?: unknown
  security?: Record<string, unknown>[]
  responses: Record<string, Response>
}

const spec = JSON.parse(readFileSync(new URL('../openapi.json', import.meta.url), 'utf8')) as {
  components: { schemas: Record<string, { enum?: string[] }> }
  paths: Record<string, Record<string, Operation>>
}

/// Every operation, as `[label, operation]`. Anything in a path item that is
/// not an operation (`parameters`, `summary`) has no `responses`.
const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
  Object.entries(item)
    .filter(([, op]) => op && typeof op === 'object' && 'responses' in op)
    .map(([verb, op]) => [`${verb.toUpperCase()} ${path}`, op] as const),
)

const refusals = (op: Operation) =>
  Object.entries(op.responses).filter(
    ([status]) => status.startsWith('4') || status.startsWith('5'),
  )

test('the document has operations to check', () => {
  assert.ok(operations.length > 30, `only ${operations.length} operations`)
})

/// A doc comment sits above one `#[utoipa::path]`, and nothing says which.
/// `/api/v1/bootstrap`'s explanation was glued onto the block above `metrics`,
/// so the published document described the Prometheus endpoint as "which
/// screen the client should open on" and said nothing at all about bootstrap.
/// Both still compiled, still served, and still generated a client.
///
/// A ratchet rather than a clean assertion: 33 operations were already
/// undocumented when this was written, and turning that into a red suite would
/// have made the check something to switch off. New ones are refused; delete a
/// name from the list as its handler gets a doc comment. Documenting the rest
/// is its own piece of work — a published contract a third-party client is
/// meant to work from should not be half silent.
const UNDOCUMENTED = new Set([
  'GET /admin/v1/collections',
  'GET /admin/v1/enrich',
  'POST /admin/v1/enrich',
  'POST /admin/v1/enrich/search',
  'GET /admin/v1/enrollments',
  'POST /admin/v1/enrollments/approve',
  'POST /admin/v1/items/{id}/match',
  'POST /admin/v1/libraries',
  'DELETE /admin/v1/libraries/{id}',
  'POST /admin/v1/libraries/{id}/collections',
  'DELETE /admin/v1/libraries/{id}/collections/{module_id}/{collection_id}',
  'GET /admin/v1/providers',
  'POST /admin/v1/providers/tmdb',
  'POST /admin/v1/providers/tvdb',
  'GET /admin/v1/satellites',
  'POST /admin/v1/satellites/{id}/disabled',
  'GET /admin/v1/sessions',
  'DELETE /admin/v1/sessions/{id}',
  'POST /admin/v1/users',
  'POST /api/v1/auth/logout',
  'POST /api/v1/auth/refresh',
  'POST /api/v1/auth/token',
  'GET /api/v1/collections',
  'GET /api/v1/items',
  'GET /api/v1/items/{id}',
  'GET /api/v1/items/{id}/artwork',
  'GET /api/v1/items/{id}/fonts',
  'GET /api/v1/items/{id}/fonts/{n}',
  'POST /api/v1/playback/sessions',
  'DELETE /api/v1/playback/sessions/{id}',
  'GET /api/v1/playback/sessions/{id}/stream',
  'PUT /api/v1/prefs',
  'POST /api/v1/setup',
])

const silent = (op: Operation) => !op.summary?.trim() && !op.description?.trim()

test('a newly published operation says what it is', () => {
  const mute = operations
    .filter(([, op]) => silent(op))
    .map(([label]) => label)
    .filter((label) => !UNDOCUMENTED.has(label))
  assert.deepEqual(mute, [])
})

/// A name left behind after its handler is documented would quietly re-open
/// the hole for whatever path gets renamed onto it.
test('the list of known-silent operations has no stale names', () => {
  const still = new Set<string>(operations.filter(([, op]) => silent(op)).map(([label]) => label))
  assert.deepEqual(
    [...UNDOCUMENTED].filter((label) => !still.has(label)),
    [],
  )
})

test('every refusal carries an ApiErrorBody, or no body at all', () => {
  // The `$ref`, not just the media type. An earlier cut of this asserted only
  // `application/json` and so would have passed a 4xx carrying some other
  // JSON schema — which is the drift it exists to catch.
  //
  // 416 is the one bodyless refusal: RFC 9110 puts the answer in
  // `Content-Range`, and a code would add nothing.
  const wrong = operations.flatMap(([label, op]) =>
    refusals(op)
      .filter(([, response]) => Object.keys(response.content ?? {}).length > 0)
      .filter(
        ([, response]) =>
          Object.keys(response.content ?? {}).join() !== 'application/json' ||
          response.content?.['application/json']?.schema?.$ref !==
            '#/components/schemas/ApiErrorBody',
      )
      .map(
        ([status, response]) =>
          `${label} ${status} -> ${Object.keys(response.content ?? {}).join()} ${
            response.content?.['application/json']?.schema?.$ref ?? '(no $ref)'
          }`,
      ),
  )
  assert.deepEqual(wrong, [])
})

test('a route that takes a body declares what a bad one produces', () => {
  // 400 a body that will not parse, 415 a wrong Content-Type, 413 one past
  // the buffer limit. All three come from the extractor, so any route with a
  // body can answer all three.
  const missing = operations.flatMap(([label, op]) =>
    op.requestBody
      ? ['400', '413', '415'].filter((s) => !(s in op.responses)).map((s) => `${label} ${s}`)
      : [],
  )
  assert.deepEqual(missing, [])
})

test('a route with a path or query parameter declares the 400 one can produce', () => {
  const missing = operations
    .filter(([, op]) => (op.parameters?.length ?? 0) > 0 && !('400' in op.responses))
    .map(([label]) => label)
  assert.deepEqual(missing, [])
})

test('a route with no body declares neither 413 nor 415', () => {
  // The inverse, and the one a sweep got wrong: nine bodyless routes — a
  // listing, an SSE stream — ended up documenting an unsupported-media-type
  // response they can never return.
  const spurious = operations.flatMap(([label, op]) =>
    op.requestBody
      ? []
      : ['413', '415'].filter((s) => s in op.responses).map((s) => `${label} ${s}`),
  )
  assert.deepEqual(spurious, [])
})

/// Which operations sit behind `require_auth` — the ones whose declared
/// security is the bearer token or the media cookie. Keyed off the document
/// rather than off "declares a 401", which was the first cut and swept in
/// `/metrics` (its own token) and `refresh` (the cookie): neither consults
/// `setup_required`, so both were made to advertise a refusal they cannot
/// return, which is the untruth the sibling tests exist to stop.
const behindRequireAuth = ([, op]: readonly [string, Operation]) =>
  (op.security ?? []).some((s) => 'bearer_auth' in s || 'media_token' in s)

test('a route behind require_auth declares the refusal that precedes it', () => {
  // `require_auth` answers 503 `setup_required` while the hub has no
  // administrator, and the main API listener is up during that window.
  // 55 operations declared the 401 and not the 503.
  const missing = operations
    .filter(behindRequireAuth)
    .filter(([, op]) => !('503' in op.responses))
    .map(([label]) => label)
  assert.deepEqual(missing, [])
})

test('and its 503 names that refusal, not only the domain one', () => {
  // Presence of a 503 key is not enough. Three routes had one describing only
  // their own meaning — "the mediahost holding the bytes is away" — so a
  // client meeting the pre-setup window read it as a mediahost problem and
  // stood by for a machine instead of routing the user to setup. OpenAPI has
  // one response per status, so a status with two meanings has to name both.
  const silent = operations
    .filter(behindRequireAuth)
    .filter(([, op]) => !(op.responses['503']?.description ?? '').includes('setup_required'))
    .map(([label]) => label)
  assert.deepEqual(silent, [])
})

test('a route that is not behind it does not claim that refusal', () => {
  const spurious = operations
    .filter((entry) => !behindRequireAuth(entry))
    .filter(([, op]) => op.responses['503']?.description?.includes('setup_required'))
    .map(([label]) => label)
  assert.deepEqual(spurious, [])
})

test('a refusal the hub can put a clock on says so in a header', () => {
  // `Retry-After` is part of the contract, and a header that is sent but not
  // declared is one a generated client cannot see. The login lockout is the
  // only refusal that carries it: the stream cap clears when a person stops
  // watching something, and a number there would be invented.
  const throttled = spec.paths['/api/v1/auth/token']?.post?.responses['429'] as
    | { headers?: Record<string, unknown> }
    | undefined
  assert.ok(throttled, 'the login route no longer declares a 429')
  assert.ok(throttled.headers?.['retry-after'], 'the 429 does not declare Retry-After')
})

test('every code a refusal can carry is published', () => {
  const codes = spec.components.schemas.ErrorCode?.enum
  assert.ok(codes && codes.length > 0, 'ErrorCode is not in the document')
  // Snake case, because that is what a client matches on and a rename is a
  // breaking change somebody should have to mean.
  assert.deepEqual(
    codes.filter((c) => !/^[a-z][a-z_]*$/.test(c)),
    [],
  )
})
