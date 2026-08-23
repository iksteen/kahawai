<script setup lang="ts">
/// Your settings. Everything here saves the moment you change it, which is
/// why every control is optimistic and every failure puts the value back —
/// see `useOptimistic`, which is where the hard part is.
import { computed, ref, watch } from 'vue'
import { useQuery, useQueryClient } from '@tanstack/vue-query'

import Armed from '../components/Armed.vue'
import Btn from '../components/Btn.vue'
import Failed from '../components/Failed.vue'
import Icon from '../components/Icon.vue'
import Ordered from '../components/Ordered.vue'
import {
  ASS_RUNGS,
  assLadder,
  bandwidthValue,
  ORIGINAL,
  stored,
  suggestions,
  validToken,
  wishlist,
} from '../domain/prefs.ts'
import {
  accountOpensubtitles,
  deleteAccountOpensubtitles,
  setAccountOpensubtitles,
} from '../api/generated/kahawai.ts'
import { addAbove, moved } from '../domain/reorder.ts'
import { notify } from '../composables/notices.ts'
import { putPref, usePrefs } from '../composables/prefs.ts'
import { sentence } from '../domain/refusal.ts'
import { useOptimistic } from '../composables/optimistic.ts'

/// Per media type, because what you want from a film is not what you want
/// from anime — HUB-33.
const MEDIA_TYPES = ['movies', 'series', 'anime'] as const

const { query, values, known } = usePrefs()
const client = useQueryClient()

/// The screen's own copy, seeded from the server's answer and edited in place.
/// An optimistic control needs somewhere to hold a value the server has not
/// confirmed, and a value put back has to land somewhere too.
watch(known, (fresh) => (values.value = { ...fresh }), { immediate: true })

const saved = ref(false)
let flashTimer: ReturnType<typeof setTimeout> | undefined
function flash() {
  saved.value = true
  clearTimeout(flashTimer)
  flashTimer = setTimeout(() => (saved.value = false), 1200)
}

/// One optimistic writer per key. They must not share a revert target: two
/// controls failing at once would put each other's values back.
const writers = new Map<string, ReturnType<typeof useOptimistic<string>>>()
function writerFor(key: string) {
  const held = writers.get(key)
  if (held) return held
  const made = useOptimistic(
    computed({
      get: () => values.value[key] ?? '',
      set: (next: string) => (values.value = { ...values.value, [key]: next }),
    }),
  )
  writers.set(key, made)
  return made
}

async function save(key: string, next: string) {
  const ok = await writerFor(key).put(next, () => putPref('', key, next))
  if (ok) flash()
}

/// A wishlist, as the list the controls work on.
function listFor(kind: 'audio' | 'subs', mediaType: string): string[] {
  return wishlist(values.value[`${kind}.${mediaType}`] ?? '', kind)
}
function saveList(kind: 'audio' | 'subs', mediaType: string, items: string[]) {
  void save(`${kind}.${mediaType}`, stored(items))
}

const typing = ref<Record<string, string>>({})
const rejected = ref<Record<string, boolean>>({})

function add(kind: 'audio' | 'subs', mediaType: string) {
  const field = `${kind}.${mediaType}`
  const token = (typing.value[field] ?? '').trim().toLowerCase()
  if (!validToken(token)) {
    rejected.value = { ...rejected.value, [field]: true }
    return
  }
  const items = listFor(kind, mediaType)
  if (items.includes(token)) {
    // Already there: nothing to save, and nothing to complain about either.
    typing.value = { ...typing.value, [field]: '' }
    return
  }
  rejected.value = { ...rejected.value, [field]: false }
  typing.value = { ...typing.value, [field]: '' }
  // Above the backstop, wherever it sits — a language added after it is never
  // reached, and the setting silently does nothing.
  saveList(kind, mediaType, addAbove(items, token, ORIGINAL))
}

function move(kind: 'audio' | 'subs', mediaType: string, from: number, to: number) {
  const next = moved(listFor(kind, mediaType), from, to)
  if (next) saveList(kind, mediaType, next)
}

function remove(kind: 'audio' | 'subs', mediaType: string, at: number) {
  const items = listFor(kind, mediaType)
  saveList(
    kind,
    mediaType,
    items.filter((_, n) => n !== at),
  )
}

/// The bandwidth box keeps its own copy, so a refusal can put the old value
/// back. Stored as the server stores it: no cap is an empty string, not "0",
/// or the local copy disagrees with the hub about the same key.
const bandwidth = ref('')
watch(
  () => values.value['bandwidth_kbps'],
  (v) => (bandwidth.value = v ?? ''),
  { immediate: true },
)

/// Read off the field rather than off the model: the box is the only control
/// here that is not committed on every keystroke, and one binding fewer
/// between what was typed and what is saved is one fewer thing to be stale.
async function saveBandwidth(typed: string) {
  bandwidth.value = typed
  const next = bandwidthValue(typed.trim() === '0' ? '' : typed)
  if (next === null) {
    notify('That is not a number of kbit/s.')
    bandwidth.value = values.value['bandwidth_kbps'] ?? ''
    return
  }
  if (next === (values.value['bandwidth_kbps'] ?? '')) return
  await save('bandwidth_kbps', next)
  bandwidth.value = values.value['bandwidth_kbps'] ?? ''
}

const ladder = computed(() => assLadder(values.value['ass_order'] ?? ''))
function moveRung(from: number, to: number) {
  const next = moved(ladder.value, from, to)
  if (next) void save('ass_order', stored(next))
}

/// HUB-31: which numbering an absolute-numbered series is shown in. The two
/// sides of one control — the title on each says what the word means, because
/// "native" does not say it on its own.
const ANIME_VIEWS = [
  { value: 'seasons', title: 'TVDB-style seasons (projected)' },
  { value: 'native', title: 'flat absolute numbering (AniDB-native)' },
] as const
/// HUB-31: which numbering an absolute-numbered series is shown in.
const animeView = computed(() => (values.value['anime_view'] === 'native' ? 'native' : 'seasons'))

/// Clicking a language makes it the first choice — the one-press version of a
/// drag, and the reason the name is a button.
function promote(kind: 'audio' | 'subs', mediaType: string, at: number) {
  const items = listFor(kind, mediaType)
  const picked = items[at]
  if (!picked) return
  saveList(kind, mediaType, [picked, ...items.filter((_, n) => n !== at)])
}

/// HUB-21. Subtitle search works without an account, on a download budget the
/// whole server shares; attaching your own spends your own instead.
///
/// The account lives in the hub's credential store, which does not read secrets
/// back — so the hub answers whether one is attached and nothing else. Both
/// fields therefore start empty on every visit, including the username: the
/// card says an account is there, not which.
const osUser = ref('')
const osPass = ref('')
const osBusy = ref(false)
const osAccount = useQuery({
  queryKey: ['account', 'opensubtitles'],
  queryFn: () => accountOpensubtitles(),
})
const osAttached = computed(() => osAccount.data.value?.configured ?? false)
/// A read that failed is not an answer. Defaulting it to false said "no
/// account" — the same quiet downgrade the hub refuses to make on its side —
/// while the viewer's subtitle searches were failing for the very reason this
/// read did: a credential the hub cannot open answers 500, not `false`.
const osUnknown = computed(() => (osAccount.isError.value ? sentence(osAccount.error.value) : ''))

async function saveAccount() {
  osBusy.value = true
  try {
    // As typed. A password is not the form's to tidy.
    await setAccountOpensubtitles({ username: osUser.value, password: osPass.value })
    osUser.value = ''
    osPass.value = ''
    await client.invalidateQueries({ queryKey: ['account', 'opensubtitles'] })
    flash()
  } catch (e) {
    notify(sentence(e))
  } finally {
    osBusy.value = false
  }
}

async function disconnect() {
  osBusy.value = true
  try {
    await deleteAccountOpensubtitles()
    await client.invalidateQueries({ queryKey: ['account', 'opensubtitles'] })
    flash()
  } catch (e) {
    notify(sentence(e))
  } finally {
    osBusy.value = false
  }
}
</script>

<template>
  <!-- A failed load used to render the page anyway, with every control showing
       its default — which reads as "these are your settings" when the truth is
       "we have no idea what your settings are". Worse than a blank screen,
       because the next thing you do is change one. -->
  <Failed
    v-if="query.isError.value"
    what="Could not load your settings."
    :message="sentence(query.error.value)"
    @retry="query.refetch()"
  />

  <main v-else-if="query.data.value">
    <div class="mb-1 flex items-baseline gap-3">
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">Settings</h1>
      <!-- Always in the layout, only sometimes visible: a chip that appears
           would shift the heading every time you changed something. -->
      <span
        class="rounded px-1.5 py-0.5 font-mono text-[11px] text-teal transition-opacity"
        :class="saved ? 'opacity-100' : 'opacity-0'"
        role="status"
      >
        saved
      </span>
    </div>
    <p class="mb-3 text-dim">Everything here saves the moment you change it.</p>

    <!-- One column, one measure. Cards 12px apart rather than 24, so the page
         reads as a stack of related settings and not as separate screens. -->
    <div class="flex max-w-[660px] flex-col gap-3">
      <!-- HUB-21. Works without an account; attaching one spends your own
           download budget instead of the server's shared one. -->
      <section class="flex flex-col gap-3 rounded-md border border-line bg-surface px-4 py-3.5">
        <div class="flex items-center gap-2">
          <span class="flex text-teal"><Icon name="download" :size="15" /></span>
          <span class="text-[14px] leading-none font-[600] capitalize">OpenSubtitles</span>
        </div>
        <p class="m-0 max-w-[560px] text-[12.5px] text-dim">
          Subtitle search works without an account, on a small download budget shared by everyone on
          this server. Attach your own opensubtitles.com account to spend your own budget instead.
          Subtitles you download are shared with everyone here.
        </p>
        <!-- In the document from the first render, like the admin panel's:
             a live region inserted together with its text is commonly
             announced by nothing. -->
        <p class="m-0 min-h-0 text-[12.5px] text-warn empty:hidden" role="status">
          {{ osUnknown ? `${osUnknown} — whether an account is attached is unknown.` : '' }}
        </p>
        <div class="flex flex-wrap items-center gap-x-2.5 gap-y-2">
          <label class="w-[76px] shrink-0 font-mono text-[12px] text-dim" for="os-user">
            account
          </label>
          <input
            id="os-user"
            v-model="osUser"
            class="min-w-[150px] flex-[1_1_170px] rounded border border-line bg-bg px-2 py-1"
            autocomplete="off"
            :placeholder="
              osAttached ? 'account configured — enter to replace' : 'opensubtitles.com username'
            "
          />
          <label class="sr-only" for="os-pass">opensubtitles.com password</label>
          <input
            id="os-pass"
            v-model="osPass"
            class="min-w-[150px] flex-[1_1_170px] rounded border border-line bg-bg px-2 py-1"
            type="password"
            autocomplete="off"
            placeholder="password"
          />
          <!-- Lights when the hub would accept it: both fields non-empty, and
               nothing else judged here. -->
          <Btn small :disabled="osBusy || !osUser || !osPass" @click="saveAccount">Save</Btn>
          <!-- Asked twice, like the admin panel's: the hub will not read the
               account back, so a stray press costs a trip to
               opensubtitles.com rather than a glance at the screen. -->
          <Armed
            v-if="osAttached"
            label="Disconnect"
            armed-label="Really disconnect?"
            :disabled="osBusy"
            title="Deletes the stored account; searches fall back to the shared budget"
            @confirm="disconnect"
          />
        </div>
      </section>

      <section class="flex flex-col gap-3 rounded-md border border-line bg-surface px-4 py-3.5">
        <div class="flex items-center gap-2">
          <span class="flex text-teal"><Icon name="play" :size="15" /></span>
          <span class="text-[14px] leading-none font-[600] capitalize">Playback</span>
        </div>
        <p class="m-0 max-w-[560px] text-[12.5px] text-dim">
          Applies wherever you play, on this account.
        </p>

        <div class="flex flex-wrap items-center gap-x-2.5 gap-y-2">
          <label class="w-[76px] shrink-0 font-mono text-[12px] text-dim" for="bandwidth">
            bandwidth
          </label>
          <!-- Shares the line rather than taking one: a fixed width here left
               the field two thirds empty beside a placeholder it could not
               show. -->
          <input
            id="bandwidth"
            v-model="bandwidth"
            class="min-w-[150px] flex-[1_1_170px] rounded border border-line bg-bg px-2 py-1 font-mono"
            type="number"
            min="0"
            placeholder="kbit/s cap (0 = none)"
            @blur="saveBandwidth(($event.target as HTMLInputElement).value)"
          />
        </div>
        <!-- An explanation belongs under the control it explains, lined up
             with it: 76px of label plus the 10px gap. -->
        <p class="mt-[-2px] ml-[86px] max-w-[480px] text-[12px] text-dim">
          A ceiling for how much data playback may use — worth setting on a metered or slow
          connection. Anything above it is re-encoded smaller; a file that cannot be re-encoded will
          refuse to play rather than stall. Leave it empty for no limit.
        </p>

        <div class="flex flex-wrap items-center gap-x-2.5 gap-y-2">
          <label class="w-[76px] shrink-0 font-mono text-[12px] text-dim" for="introdb">
            skip data
          </label>
          <input
            id="introdb"
            class="accent-teal"
            type="checkbox"
            :checked="values['introdb'] === '1'"
            @change="save('introdb', ($event.target as HTMLInputElement).checked ? '1' : '')"
          />
          <span class="text-[12.5px]">Fetch skip times from theintrodb.org</span>
        </div>
        <p class="mt-[-2px] ml-[86px] max-w-[480px] text-[12px] text-dim">
          When this server has measured nothing for what you are playing, the player asks
          <a
            class="text-teal underline"
            href="https://theintrodb.org"
            target="_blank"
            rel="noopener"
            >TheIntroDB</a
          >, a community database — directly from your browser, so that site sees your address, this
          server's web address, and the provider id, episode numbering and running time of what you
          are watching. Off unless you turn it on.
        </p>
      </section>

      <section class="flex flex-col gap-3 rounded-md border border-line bg-surface px-4 py-3.5">
        <!-- Not a card NAME: those are capitalised because they are proper
             nouns and media types. This is a sentence, and `capitalize` was
             rendering it "Styled Subtitles". -->
        <span class="text-[12.5px] font-[600]">Styled subtitles</span>
        <p class="m-0 max-w-[560px] text-[12.5px] text-dim">
          How styled subtitles reach a player that cannot draw them itself, in the order to try. The
          server takes the first rung this client and this server can actually serve.
        </p>
        <!-- `fixed`: the order expresses priority, never removal. Every rung is
             always present, so a ✕ on each one offered something that does not
             exist (owner decision, 2026-08-03). -->
        <Ordered
          :items="ladder"
          fixed
          label="Styled subtitle fallbacks, in order"
          :display="(rung) => ASS_RUNGS[rung as keyof typeof ASS_RUNGS].name"
          :note="(rung) => ASS_RUNGS[rung as keyof typeof ASS_RUNGS].note"
          @move="moveRung"
        />
      </section>

      <!-- Which tracks to start with, per media type: what you want from a
           film is not what you want from anime (HUB-33). -->
      <div class="mt-1">
        <span class="text-[12.5px] font-[600]">Which tracks to start with</span>
        <p class="m-0 mt-2 max-w-[560px] text-[12.5px] text-dim">
          When you open something, the first language in each list that the file actually has is the
          one that plays. Drag a language to move it, or click it to make it your first choice.
          <span class="font-mono text-teal">original</span> means whatever language the title was
          made in. Picking a different track while watching only affects that title, and it wins
          over these.
        </p>
      </div>

      <section
        v-for="mediaType in MEDIA_TYPES"
        :key="mediaType"
        class="flex flex-col gap-3 rounded-md border border-line bg-surface px-4 py-3.5"
      >
        <div class="flex items-center gap-2">
          <span class="flex text-teal">
            <Icon :name="mediaType === 'movies' ? 'movie' : 'show'" :size="15" />
          </span>
          <span class="text-[14px] leading-none font-[600] capitalize">{{ mediaType }}</span>
        </div>

        <!-- Anime's presentation first: it decides how the episode lists on
             every other screen are numbered, which is a bigger difference than
             a track order. -->
        <template v-if="mediaType === 'anime'">
          <div class="flex flex-wrap items-center gap-x-2.5 gap-y-2">
            <span id="anime-view" class="w-[76px] shrink-0 font-mono text-[12px] text-dim">
              view
            </span>
            <!-- A two-state choice reads as one control, not two chips. The
                 unpicked side keeps full-strength text: dimming it is how a
                 disabled control looks, and it is not disabled — it is the
                 other half of the choice. -->
            <span class="flex flex-wrap gap-1.5" role="group" aria-labelledby="anime-view">
              <button
                v-for="option in ANIME_VIEWS"
                :key="option.value"
                class="inline-flex min-h-7 cursor-pointer items-center rounded border px-[11px] py-1 font-mono text-[12px] tracking-[0.02em]"
                :class="
                  animeView === option.value
                    ? 'border-teal bg-teal font-[600] text-bg'
                    : 'border-dim bg-surface text-text hover:border-prose hover:bg-hover'
                "
                type="button"
                :aria-pressed="animeView === option.value"
                :title="option.title"
                @click="save('anime_view', option.value)"
              >
                {{ option.value }}
              </button>
            </span>
          </div>
          <p class="mt-[-2px] ml-[86px] max-w-[480px] text-[12px] text-dim">
            {{
              animeView === 'seasons'
                ? 'Numbered in seasons, the way most people know these shows.'
                : 'Numbered straight through, the way they were broadcast.'
            }}
          </p>
        </template>

        <!-- Label, pills and the way to add one, all on the same line — a
             language is a word, and a dozen of them belong in a row. -->
        <div
          v-for="kind in ['audio', 'subs'] as const"
          :key="kind"
          class="flex flex-wrap items-center gap-x-2.5 gap-y-2"
        >
          <span class="w-[76px] shrink-0 font-mono text-[12px] text-dim">
            {{ kind === 'audio' ? 'audio' : 'subtitles' }}
          </span>
          <span
            v-if="listFor(kind, mediaType).length === 0"
            class="flex min-h-[1.6rem] flex-[1_1_240px] items-center text-[12px] text-dimmer"
          >
            no subtitles
          </span>
          <!-- The pills take the slack, which is what puts the add box against
               the right edge of the row rather than against the last pill. -->
          <Ordered
            v-else
            chips
            class="min-h-[1.6rem] flex-[1_1_240px]"
            :items="listFor(kind, mediaType)"
            :label="`${kind === 'audio' ? 'Audio' : 'Subtitle'} languages for ${mediaType}, in order`"
            :pinned="kind === 'audio' ? [ORIGINAL] : []"
            @move="(from, to) => move(kind, mediaType, from, to)"
            @remove="(at) => remove(kind, mediaType, at)"
            @promote="(at) => promote(kind, mediaType, at)"
          />
          <label class="sr-only" :for="`${kind}-${mediaType}`">
            Add a language for {{ mediaType }}
          </label>
          <!-- Dashed, because it is an opening rather than a value. -->
          <input
            :id="`${kind}-${mediaType}`"
            v-model="typing[`${kind}.${mediaType}`]"
            class="w-[70px] flex-none rounded border border-dashed bg-bg px-2 py-1 font-mono text-[12px]"
            :class="rejected[`${kind}.${mediaType}`] ? 'border-warn' : 'border-line'"
            :aria-invalid="rejected[`${kind}.${mediaType}`] || undefined"
            :list="`langs-${kind}-${mediaType}`"
            placeholder="add…"
            @keydown.enter.prevent="add(kind, mediaType)"
            @blur="typing[`${kind}.${mediaType}`]?.trim() && add(kind, mediaType)"
          />
          <datalist :id="`langs-${kind}-${mediaType}`">
            <option
              v-for="token in suggestions(listFor(kind, mediaType), kind)"
              :key="token"
              :value="token"
            />
          </datalist>
          <span v-if="rejected[`${kind}.${mediaType}`]" class="text-[12px] text-warn" role="alert">
            Two or three letters, like “en” or “nld”.
          </span>
        </div>
      </section>
    </div>
  </main>
</template>
