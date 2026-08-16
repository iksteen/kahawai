<script setup lang="ts">
/// Credentials for the services that identify and describe the media.
///
/// Grouped by what they are FOR, not by who runs them: an admin comes here
/// because anime is matching badly, not because they were thinking about
/// AniDB. The provider name is the row label.
import { computed, ref, watch } from 'vue'
import { useQuery, useQueryClient } from '@tanstack/vue-query'

import Btn from '../../components/Btn.vue'
import Icon from '../../components/Icon.vue'
import Ordered from '../../components/Ordered.vue'
import {
  adminEnrichRun,
  adminEnrichStatus,
  adminProviders,
  adminSetAnidb,
  adminSetChain,
  adminSetTmdb,
  adminSetTvdb,
} from '../../api/generated/kahawai.ts'
import { moved } from '../../domain/reorder.ts'
import { notify } from '../../composables/notices.ts'
import { POLL_MS } from '../../composables/admin.ts'
import { sentence } from '../../domain/refusal.ts'

const props = defineProps<{
  act: (what: () => Promise<unknown>) => Promise<boolean>
  refused: (why: string) => void
}>()

const client = useQueryClient()

/// Its own two reads rather than the panel's six: nothing else on the page
/// wants them, and a provider key is not something the satellites tab should
/// pay for on every poll.
const providers = useQuery({
  queryKey: ['admin', 'providers'],
  queryFn: () => adminProviders(),
  refetchInterval: POLL_MS,
})
const enrich = useQuery({
  queryKey: ['admin', 'enrich'],
  queryFn: () => adminEnrichStatus(),
  refetchInterval: POLL_MS,
})

async function reload() {
  await Promise.all([
    client.invalidateQueries({ queryKey: ['admin', 'providers'] }),
    client.invalidateQueries({ queryKey: ['admin', 'enrich'] }),
  ])
}

/// This panel polls, and a read that swallowed its failure left the credentials
/// and the match order on screen looking current.
const readError = computed(() =>
  providers.isError.value
    ? sentence(providers.error.value)
    : enrich.isError.value
      ? sentence(enrich.error.value)
      : '',
)

/// Said once, on the way into failure and once on the way out. A notice every
/// fifteen seconds would be worse than silence, and the line above goes quiet
/// when it recovers — with nothing to say that it has.
let failing = false
watch(readError, (why) => {
  if (why && !failing) {
    failing = true
    notify('Cannot reach the hub — what is shown here may be out of date.')
  } else if (!why && failing) {
    failing = false
    notify('Provider settings are up to date again.')
  }
})

const tmdb = ref('')
const tvdb = ref({ key: '', pin: '' })
const anidb = ref({ username: '', password: '', udp: '' })

const configured = computed(() => providers.data.value)
/// Whether ANY provider can answer. The enrich button used to read TMDB's flag
/// alone, so a series-only deployment had a permanently greyed button and no
/// explanation of why.
const anyProvider = computed(
  () =>
    !!configured.value &&
    (configured.value.tmdb.configured ||
      configured.value.tvdb.configured ||
      configured.value.anidb.configured),
)

async function saveTmdb() {
  if (!(await props.act(() => adminSetTmdb({ api_key: tmdb.value.trim() })))) return
  tmdb.value = ''
  notify('TMDB key saved — enrichment started.')
  void reload()
}

async function saveTvdb() {
  const { key, pin } = tvdb.value
  const ok = await props.act(() =>
    adminSetTvdb(pin.trim() ? { api_key: key.trim(), pin: pin.trim() } : { api_key: key.trim() }),
  )
  if (!ok) return
  tvdb.value = { key: '', pin: '' }
  notify('TVDB key saved — enrichment started.')
  void reload()
}

async function saveAnidb() {
  const { username, password, udp } = anidb.value
  let verified = false
  let why: string | null | undefined
  const ok = await props.act(async () => {
    const answer = await adminSetAnidb(
      udp.trim()
        ? { username: username.trim(), password, udp_api_key: udp.trim() }
        : { username: username.trim(), password },
    )
    verified = answer.verified
    why = answer.error
  })
  if (!ok) return
  anidb.value = { username: '', password: '', udp: '' }
  // Saved and verified are two different answers, and the hub gives both. A
  // credential the hub could not log in with is saved and useless.
  notify(
    verified
      ? 'AniDB account verified — enrichment started.'
      : `AniDB saved but login failed: ${why ?? 'unknown'}`,
  )
  void reload()
}

/// HUB-5: which provider wins a field, per media type. Earlier providers own a
/// field; later ones only fill what the earlier left empty. Applying re-merges
/// from answers already on disk — no provider is contacted, so this is safe to
/// try and trivially reversible.
///
/// A DRAFT, applied by a button, because a chain is an ordering and half an
/// ordering is not a state worth writing.
const draft = ref<Record<string, string[]>>({})
const applying = ref<string | null>(null)

const chains = computed(() => configured.value?.chains ?? {})
const order = (type: string) => draft.value[type] ?? chains.value[type]?.order ?? []
const dirty = (type: string) =>
  JSON.stringify(order(type)) !== JSON.stringify(chains.value[type]?.order ?? [])

function move(type: string, from: number, to: number) {
  const next = moved(order(type), from, to)
  if (next) draft.value = { ...draft.value, [type]: next }
}

function reset(type: string) {
  const { [type]: _dropped, ...rest } = draft.value
  draft.value = rest
}

async function apply(type: string) {
  applying.value = type
  try {
    if (!(await props.act(() => adminSetChain(type, { order: order(type) })))) return
    reset(type)
    notify(`${type}: provider order applied — metadata re-merged.`)
    void reload()
  } finally {
    applying.value = null
  }
}

async function run() {
  if (!(await props.act(() => adminEnrichRun()))) return
  void reload()
}
</script>

<template>
  <!-- In the document from the first render, like the panel's own two: a live
       region inserted together with its text is commonly announced by
       nothing. -->
  <p class="mb-3 min-h-0 text-warn empty:mb-0" role="status">
    {{ readError ? `${readError} — what is shown here may be out of date.` : '' }}
  </p>

  <div class="flex flex-col gap-4">
    <section class="rounded border border-line bg-surface p-3" aria-labelledby="providers-movies">
      <h2
        id="providers-movies"
        class="mb-3 flex items-center gap-2 text-[14px] leading-none font-[600] capitalize"
      >
        <Icon name="movie" />
        Movies &amp; series
      </h2>
      <div class="mb-2 flex flex-wrap items-center gap-2">
        <label class="w-20 font-mono text-[12px] text-dim" for="tmdb-key">TMDB</label>
        <input
          id="tmdb-key"
          v-model="tmdb"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          :placeholder="
            configured?.tmdb.configured ? 'key configured — paste to replace' : 'API key'
          "
        />
        <Btn small :disabled="!tmdb.trim()" @click="saveTmdb">Save</Btn>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <label class="w-20 font-mono text-[12px] text-dim" for="tvdb-key">TheTVDB</label>
        <input
          id="tvdb-key"
          v-model="tvdb.key"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          :placeholder="
            configured?.tvdb.configured ? 'key configured — paste to replace' : 'API key'
          "
        />
        <label class="sr-only" for="tvdb-pin">TheTVDB PIN</label>
        <input
          id="tvdb-pin"
          v-model="tvdb.pin"
          class="w-48 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          placeholder="PIN, if your key needs one"
        />
        <Btn small :disabled="!tvdb.key.trim()" @click="saveTvdb">Save</Btn>
      </div>
    </section>

    <section class="rounded border border-line bg-surface p-3" aria-labelledby="providers-anime">
      <h2
        id="providers-anime"
        class="mb-3 flex items-center gap-2 text-[14px] leading-none font-[600] capitalize"
      >
        <Icon name="show" />
        Anime
        <span
          class="rounded border px-1.5 py-0.5 font-mono text-[11px]"
          :class="configured?.anidb.configured ? 'border-teal text-teal' : 'border-line text-dim'"
        >
          {{ configured?.anidb.configured ? 'account attached' : 'title search only' }}
        </span>
      </h2>
      <p class="mb-2 max-w-[80ch] text-dim">
        An AniDB account enables exact file matching — the precise episode, release group and
        version. Without one, matching falls back to searching by title.
      </p>
      <div class="mb-2 flex flex-wrap items-center gap-2">
        <label class="w-20 font-mono text-[12px] text-dim" for="anidb-user">AniDB</label>
        <input
          id="anidb-user"
          v-model="anidb.username"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          autocomplete="off"
          :placeholder="
            configured?.anidb.configured ? 'account configured — enter to replace' : 'username'
          "
        />
        <label class="sr-only" for="anidb-pass">AniDB password</label>
        <input
          id="anidb-pass"
          v-model="anidb.password"
          class="w-48 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          placeholder="password"
        />
        <Btn small :disabled="!anidb.username.trim() || !anidb.password" @click="saveAnidb">
          Save
        </Btn>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <span class="w-20" aria-hidden="true" />
        <label class="sr-only" for="anidb-udp">AniDB UDP API key</label>
        <input
          id="anidb-udp"
          v-model="anidb.udp"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          placeholder="UDP API key — optional, encrypts the session"
        />
      </div>
      <p class="mt-2 text-dim">AniList and the AniDB↔TVDB mapping need no key.</p>
    </section>

    <section
      v-if="Object.keys(chains).length"
      class="rounded border border-line bg-surface p-3"
      aria-labelledby="providers-order"
    >
      <h2
        id="providers-order"
        class="mb-3 flex items-center gap-2 text-[14px] leading-none font-[600] capitalize"
      >
        <Icon name="grip" />
        Matching order
      </h2>
      <p class="mb-3 max-w-[80ch] text-dim">
        The first provider to supply a field owns it; the rest fill what it left empty. Applying
        re-merges answers already on disk — instant, and no provider is contacted.
      </p>
      <div v-for="(_chain, type) in chains" :key="type" class="mb-3">
        <div class="mb-1 flex items-center gap-2">
          <span class="font-mono text-[12px] text-dim">{{ type }}</span>
          <span v-if="order(type).length < 2" class="text-dim">only one provider</span>
          <Btn
            small
            class="ml-auto"
            :disabled="!dirty(type) || applying === type"
            @click="apply(type)"
          >
            {{ applying === type ? 'Applying…' : 'Apply' }}
          </Btn>
          <Btn v-if="dirty(type)" ghost small @click="reset(type)">Reset</Btn>
        </div>
        <!-- Every entry pinned: a chain is a precedence over the providers
             there ARE, so there is nothing to remove from it. -->
        <Ordered
          :items="order(type)"
          :pinned="order(type)"
          :label="`Provider precedence for ${type}`"
          @move="(from, to) => move(type, from, to)"
        />
      </div>
    </section>

    <!-- Library-wide, so it sits under the cards rather than in one. -->
    <div class="flex flex-wrap items-center gap-3">
      <Btn
        ghost
        small
        :disabled="!anyProvider || (enrich.data.value?.running ?? false)"
        @click="run"
      >
        {{ enrich.data.value?.running ? 'Enriching…' : 'Enrich now' }}
      </Btn>
      <!-- In text, not in a `title` on the disabled button: a disabled button
           is out of the tab order, so its tooltip is unreachable by exactly the
           people who most need the sentence. -->
      <span v-if="!anyProvider" class="text-dim">Configure a metadata provider first.</span>
      <span
        v-if="enrich.data.value"
        class="font-mono text-[12px]"
        :class="enrich.data.value.running ? 'text-teal' : 'text-dim'"
      >
        {{ enrich.data.value.matched }} matched · {{ enrich.data.value.weak }} weak ·
        {{ enrich.data.value.missed }} missed
      </span>
    </div>
  </div>
</template>
