import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { readFile, rename, writeFile } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const webRoot = dirname(dirname(fileURLToPath(import.meta.url)))
const repoRoot = dirname(webRoot)
const extension = 'x-kahawai-source-sha256'
const openapiPath = 'web/openapi.json'
const inputs = [
  'Cargo.lock',
  'Cargo.toml',
  'crates/kahawai-core/Cargo.toml',
  'crates/kahawai-core/src/media.rs',
  'crates/kahawai-hub/Cargo.toml',
  'crates/kahawai-hub/src/api.rs',
  'crates/kahawai-hub/src/auth.rs',
  'crates/kahawai-hub/src/enrich.rs',
  'crates/kahawai-hub/src/error.rs',
  'crates/kahawai-hub/src/grants.rs',
  'crates/kahawai-hub/src/metrics.rs',
  'crates/kahawai-hub/src/opensubtitles.rs',
  'crates/kahawai-hub/src/registry.rs',
  'crates/kahawai-hub/src/sessions.rs',
  'crates/kahawai-hub/src/subtitles.rs',
  'crates/kahawai-hub/src/tracks.rs',
  'crates/kahawai-media/Cargo.toml',
  'crates/kahawai-media/src/negotiate.rs',
]

const args = new Set(process.argv.slice(2))
const staged = args.has('--staged')
const write = args.has('--write')
const check = args.has('--check')
const writeIndex = process.argv.indexOf('--write')
const targetArg = writeIndex === -1 ? undefined : process.argv[writeIndex + 1]
if (write === check || (write && staged)) {
  throw new Error('usage: openapi-fingerprint.mjs --check [--staged] | --write <openapi.json>')
}

const stagedFile = (path) =>
  execFileSync('git', ['show', `:${path}`], { cwd: repoRoot, encoding: null })
const workingFile = (path) => readFile(join(repoRoot, path))

if (staged) {
  const changed = execFileSync('git', ['diff', '--cached', '--name-only', '--diff-filter=ACMR'], {
    cwd: repoRoot,
    encoding: 'utf8',
  })
    .trim()
    .split('\n')
  if (!changed.some((path) => path === openapiPath || inputs.includes(path))) process.exit(0)
}

const source = staged ? stagedFile : workingFile
const hash = createHash('sha256')
for (const path of inputs) {
  hash.update(path)
  hash.update('\0')
  hash.update(await source(path))
  hash.update('\0')
}
const expected = hash.digest('hex')

if (write) {
  if (!targetArg) throw new Error('--write requires an OpenAPI JSON path')
  const target = resolve(webRoot, targetArg)
  const document = JSON.parse(await readFile(target, 'utf8'))
  delete document[extension]
  const stamped = {
    openapi: document.openapi,
    info: document.info,
    [extension]: expected,
    ...Object.fromEntries(
      Object.entries(document).filter(([key]) => key !== 'openapi' && key !== 'info'),
    ),
  }
  const temporary = `${target}.fingerprint.tmp`
  await writeFile(temporary, `${JSON.stringify(stamped, null, 2)}\n`)
  await rename(temporary, target)
  process.exit(0)
}

const document = JSON.parse(await source(openapiPath))
if (document[extension] !== expected) {
  const where = staged ? 'staged API sources' : 'API sources'
  console.error(`web/openapi.json is stale relative to the ${where}.`)
  console.error('Run `npm --prefix web run api:export`, then stage web/openapi.json.')
  process.exit(1)
}
