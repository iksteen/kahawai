<script setup lang="ts">
/// Credentials for the services that identify and describe the media.
///
/// Grouped by what they are FOR, not by who runs them: an admin comes here
/// because anime is matching badly, not because they were thinking about
/// AniDB. The provider name is the row label.
import { computed, ref, watch } from 'vue'
import { useQuery, useQueryClient } from '@tanstack/vue-query'

import Armed from '../../components/Armed.vue'
import Btn from '../../components/Btn.vue'
import Icon from '../../components/Icon.vue'
import Ordered from '../../components/Ordered.vue'
import {
  adminDisconnectProvider,
  adminEnrichRun,
  adminEnrichStatus,
  adminProviders,
  adminSegmentsStatus,
  adminSegmentsRun,
  adminSetAnidb,
  adminSetChain,
  adminSetFanart,
  adminSetTheaudiodb,
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
const segments = useQuery({
  queryKey: ['admin', 'segments'],
  queryFn: () => adminSegmentsStatus(),
  refetchInterval: POLL_MS,
})
const detecting = ref(false)
const canDetect = computed(
  () => segments.data.value?.collections.some((c) => c.connected && c.enabled !== false) ?? false,
)
async function detect() {
  if (detecting.value) return
  detecting.value = true
  try {
    await props.act(async () => {
      const result = await adminSegmentsRun()
      notify(
        `Skip-point discovery requested on ${result.asked} mediahosts${result.unavailable ? `; ${result.unavailable} unavailable` : ''}.`,
      )
    })
  } finally {
    detecting.value = false
    void client.invalidateQueries({ queryKey: ['admin', 'segments'] })
  }
}

async function reload() {
  await Promise.all([
    client.invalidateQueries({ queryKey: ['admin', 'providers'] }),
    client.invalidateQueries({ queryKey: ['admin', 'enrich'] }),
    client.invalidateQueries({ queryKey: ['admin', 'segments'] }),
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
const fanart = ref('')
const theaudiodb = ref('')
const tvdb = ref({ key: '', pin: '' })
const anidb = ref({ username: '', password: '', udp: '' })

const configured = computed(() => providers.data.value)
/// Whether ANY provider can answer. The enrich button used to read TMDB's flag
/// alone, so a series-only deployment had a permanently greyed button and no
/// explanation of why.
const anyProvider = computed(() => (configured.value?.available?.length ?? 0) > 0)

async function saveTmdb() {
  if (!(await props.act(() => adminSetTmdb({ api_key: tmdb.value })))) return
  tmdb.value = ''
  notify('TMDB key saved — enrichment started.')
  void reload()
}

async function saveFanart() {
  if (!(await props.act(() => adminSetFanart({ client_key: fanart.value })))) return
  fanart.value = ''
  notify('Fanart.tv key saved — artist artwork prefetch started.')
  void reload()
}

async function saveTheAudioDb() {
  if (!(await props.act(() => adminSetTheaudiodb({ api_key: theaudiodb.value })))) return
  theaudiodb.value = ''
  notify('TheAudioDB premium key saved — artist artwork prefetch started.')
  void reload()
}

async function saveTvdb() {
  const { key, pin } = tvdb.value
  const ok = await props.act(() => adminSetTvdb(pin ? { api_key: key, pin } : { api_key: key }))
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
    // Sent as typed. The UDP key is a cipher input, and a form that quietly
    // trims one stores a key AniDB will not decrypt with.
    const answer = await adminSetAnidb(
      udp ? { username, password, udp_api_key: udp } : { username, password },
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

/// Asked twice (see `Armed`): nothing here can show what is about to go — a
/// stored key is never read back — so a stray press costs a trip to the
/// provider's site to fetch it again.
async function disconnect(
  provider: 'tmdb' | 'tvdb' | 'anidb' | 'fanart' | 'theaudiodb',
  name: string,
) {
  if (!(await props.act(() => adminDisconnectProvider(provider)))) return
  notify(provider === 'theaudiodb' ? 'TheAudioDB reset to its free key.' : `${name} disconnected.`)
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
    notify(
      `${type}: provider order applied — supplement order updated. Existing matches are unchanged.`,
    )
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
        <Armed
          v-if="configured?.tmdb.configured"
          label="Disconnect"
          armed-label="Really disconnect?"
          name="Disconnect TMDB"
          armed-name="Really disconnect TMDB?"
          title="Deletes the stored TMDB key from this hub"
          @confirm="disconnect('tmdb', 'TMDB')"
        />
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
        <Armed
          v-if="configured?.tvdb.configured"
          label="Disconnect"
          armed-label="Really disconnect?"
          name="Disconnect TheTVDB"
          armed-name="Really disconnect TheTVDB?"
          title="Deletes the stored TheTVDB key from this hub"
          @confirm="disconnect('tvdb', 'TheTVDB')"
        />
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
        <Btn small :disabled="!anidb.username.trim() || !anidb.password" @click="saveAnidb">
          Save
        </Btn>
        <Armed
          v-if="configured?.anidb.configured"
          label="Disconnect"
          armed-label="Really disconnect?"
          name="Disconnect AniDB"
          armed-name="Really disconnect AniDB?"
          title="Deletes the stored AniDB account from this hub"
          @confirm="disconnect('anidb', 'AniDB')"
        />
      </div>
      <p class="mt-2 text-dim">AniList and the AniDB↔TVDB mapping need no key.</p>
    </section>

    <section class="rounded border border-line bg-surface p-3" aria-labelledby="providers-music">
      <h2
        id="providers-music"
        class="mb-3 flex items-center gap-2 text-[14px] leading-none font-[600] capitalize"
      >
        <Icon name="album" />
        Music artwork
      </h2>
      <p class="mb-2 max-w-[80ch] text-dim">
        Fanart.tv supplies Album Artist portraits first; TheAudioDB fills its gaps. The hub
        downloads and sizes every portrait in the background so browsing remains local when either
        provider is unavailable. Fanart calls its personal API key a client key. TheAudioDB works
        with its free public key unless you supply a premium key.
      </p>
      <div class="mb-2 flex flex-wrap items-center gap-2">
        <label class="w-20 font-mono text-[12px] text-dim" for="fanart-key">Fanart.tv</label>
        <input
          id="fanart-key"
          v-model="fanart"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          :placeholder="
            configured?.fanart?.configured
              ? 'key configured — paste to replace'
              : 'personal API key'
          "
        />
        <Btn small :disabled="!fanart" @click="saveFanart">Save</Btn>
        <Armed
          v-if="configured?.fanart?.configured"
          label="Disconnect"
          armed-label="Really disconnect?"
          name="Disconnect Fanart.tv"
          armed-name="Really disconnect Fanart.tv?"
          title="Deletes the stored Fanart.tv key from this hub"
          @confirm="disconnect('fanart', 'Fanart.tv')"
        />
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <label class="w-20 font-mono text-[12px] text-dim" for="theaudiodb-key">TheAudioDB</label>
        <input
          id="theaudiodb-key"
          v-model="theaudiodb"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          type="password"
          autocomplete="off"
          :placeholder="
            configured?.theaudiodb?.premium_key_configured
              ? 'premium key configured — paste to replace'
              : 'premium key — free key is active'
          "
        />
        <Btn small :disabled="!theaudiodb" @click="saveTheAudioDb">Save premium key</Btn>
        <Armed
          v-if="configured?.theaudiodb?.premium_key_configured"
          label="Use free key"
          armed-label="Really use free key?"
          name="Reset TheAudioDB to free key"
          armed-name="Really reset TheAudioDB to its free key?"
          title="Deletes the stored premium key and restores TheAudioDB's public free key"
          @confirm="disconnect('theaudiodb', 'TheAudioDB')"
        />
      </div>
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
        The selected record supplies metadata first. This order chooses future automatic matches and
        fills missing fields from verified supplements. Applying re-merges answers already on disk —
        instant, and no provider is contacted.
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
    <section aria-labelledby="skip-points" class="rounded border border-line bg-surface p-3">
      <h2 id="skip-points" class="mb-3 text-[14px] font-[600]">Skip points</h2>
      <div class="flex flex-wrap items-center gap-3">
        <Btn
          ghost
          small
          :disabled="detecting || !canDetect || segments.isError.value"
          @click="detect"
        >
          {{ detecting ? 'Requesting…' : 'Find skip points now' }}
        </Btn>
        <p class="text-dim">Mediahosts find intros, recaps and credits in the background.</p>
      </div>
      <p v-if="segments.isError.value" class="mt-2 text-warn">
        Could not read skip-point status: {{ sentence(segments.error.value) }}
        <Btn ghost small @click="segments.refetch()">Try again</Btn>
      </p>
      <p v-else-if="!segments.data.value" class="mt-2 text-dim">Loading skip-point status…</p>
      <template v-else>
        <p v-if="!segments.data.value.collections.length" class="mt-2 text-dim">
          No series or anime collections.
        </p>
        <ul v-else class="mt-2 flex flex-col gap-1">
          <li
            v-for="collection in segments.data.value.collections"
            :key="collection.collection_id"
            class="break-words text-[13px]"
          >
            {{ collection.mediahost_name }}/{{ collection.name }} ·
            <span v-if="!collection.connected" class="text-warn">offline</span>
            <span v-else-if="collection.enabled === false" class="text-dim"
              >detection disabled on this mediahost</span
            >
            <span v-else-if="collection.pending_sources == null" class="text-dim"
              >waiting for mediahost status</span
            >
            <span v-else class="text-dim">
              {{ collection.pending_sources }} sources awaiting analysis
              <template v-if="collection.enabled == null">
                · detection setting not reported</template
              >
            </span>
          </li>
        </ul>
        <p v-if="segments.data.value.collections.length" class="mt-2 text-[12px] text-dim">
          Last reported counts; only sources with enough episodes to compare are included.
        </p>
      </template>
    </section>
  </div>
</template>
