<script setup lang="ts">
/// One item's page: a film, a series, or an episode.
///
/// Three failures live here and they are three different things — see
/// `composables/item.ts`. The one that takes the screen is the item itself;
/// the children's failure is a line where the list would be; and anything you
/// asked for that did not work is a line on a page that is otherwise intact.
///
/// An album's page needs the queue that outlives it, so it lands with the
/// queue in phase 12.
import { computed, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Attribution from '../components/Attribution.vue'
import Btn from '../components/Btn.vue'
import DetailHead from '../components/DetailHead.vue'
import Failed from '../components/Failed.vue'
import Icon from '../components/Icon.vue'
import { childCount, continueAt, ordered, seasonsIn } from '../domain/detail.ts'
import {
  deliveryPlan,
  groupSources,
  planRow,
  size,
  subtitleChip,
  subtitleChipTitle,
} from '../domain/source.ts'
import {
  episodeOf,
  projecting,
  resumeMs,
  seasonLabel,
  seasonOf,
  seLabel,
  watchedPct,
} from '../domain/label.ts'
import { adminItemLog } from '../api/generated/kahawai.ts'
import { notify } from '../composables/notices.ts'
import { loadMask } from '../api/capabilities.ts'
import { maskSummary } from '../domain/capability-mask.ts'
import { sentence } from '../domain/refusal.ts'
import { saveAs } from '../api/download.ts'
import { seasonSegment } from '../domain/routes.ts'
import { whoAmI } from '../api/session.ts'
import { useChildren, useItem, useWatched } from '../composables/item.ts'
import { useQueue } from '../composables/queue.ts'

const route = useRoute()
const router = useRouter()

const id = computed(() => String(route.params.id ?? ''))
const library = computed(() => String(route.params.library ?? ''))

const query = useItem(id)
const item = computed(() => query.data.value)
const children = useChildren(item)
const { mark, busy } = useWatched()

/// HUB-31: the projection is a preference, and it is not read yet — phase 10
/// brings Settings and with it `anime_view`. Until then the projected view is
/// the default, which is what the old client defaults to.
const projected = computed(() => projecting('seasons', children.data.value ?? []))
const episodes = computed(() => ordered(children.data.value ?? [], projected.value))
const seasons = computed(() => seasonsIn(episodes.value, projected.value))
const next = computed(() => continueAt(children.data.value ?? null))

/// Hierarchical: an episode goes up to its series, everything else to the
/// library the URL came from — the one that was navigated from, which is the
/// only thing that knows, because a collection can be in more than one.
const up = computed(() => {
  const kind = item.value?.kind
  if (kind === 'episode' && item.value?.parent_id) {
    return { label: `← ${item.value.show_title ?? 'Series'}`, id: item.value.parent_id }
  }
  return { label: '← Library', id: null }
})
function goUp() {
  if (up.value.id) {
    void router.push({ name: 'detail', params: { library: library.value, id: up.value.id } })
  } else {
    void router.push({ name: 'library', params: { library: library.value } })
  }
}

/// What playback would pick: the hub orders the list that way, so the first
/// work in it is the one Play uses.
const works = computed(() => groupSources(item.value?.sources ?? []))
const best = computed(() => works.value[0]?.parts[0])

/// What the stored capability mask changes, if anything.
const masked = maskSummary(loadMask())

/// OPS-10: the LAST session for this item, whoever played it.
///
/// The point is debugging a report from somebody else, after they have closed
/// the player — so it is on the item, not on the session, and it is here
/// rather than in the admin panel because this is the page you are on when
/// somebody says "this one would not play".
const me = whoAmI()

/// A record's actions are the queue's. `playAlbum` replaces what is playing and
/// `appendAlbum` does not — both are what somebody asked for, and neither is
/// the other.
const queue = useQueue()
const tracks = computed(() => children.data.value ?? [])
/// Which track of THIS record is playing, so the list can mark it. By id: the
/// queue may hold another record entirely.
const nowPlaying = computed(() => queue.playing.value?.track.id ?? null)

/// Why the two actions cannot be pressed, or '' when they can. Said out loud
/// as well as in a title: a disabled button is out of the tab order, so its
/// tooltip is unreachable by exactly the people who need the sentence.
const whyNoTracks = computed(() => {
  if (tracks.value.length) return ''
  if (children.isError.value) return 'The track list could not be read.'
  if (children.isPending.value) return 'Still reading the track list…'
  return 'This record has no tracks.'
})
async function itemLog() {
  try {
    saveAs(`item-${id.value}.log`, await adminItemLog(id.value))
  } catch (cause) {
    notify(`Could not download the session log: ${sentence(cause)}`)
  }
}

/// A re-ask that failed under a page that is still standing. Not the screen:
/// what is on it is a moment out of date, not wrong.
watch(
  () => query.isError.value && item.value !== undefined,
  (stale) => stale && notify(`Could not re-read this item: ${sentence(query.error.value)}`),
)
const resumeAt = computed(() => (item.value ? resumeMs(item.value) : 0))

const subline = computed(() => {
  const it = item.value
  if (!it) return ''
  if (it.kind === 'show') return childCount(children.data.value ?? null, 'episode', 'episodes')
  if (it.kind === 'album') {
    return [it.artist, childCount(children.data.value ?? null, 'track', 'tracks')]
      .filter(Boolean)
      .join(' · ')
  }
  return [
    it.kind === 'episode' && it.show_title ? it.show_title : '',
    it.kind === 'episode' ? seLabel(it.season, it.episode, it.episode_end) : '',
    it.metadata?.genres?.length ? it.metadata.genres.join(' · ') : '',
  ]
    .filter(Boolean)
    .join(' · ')
})

function play(fromStart = false) {
  void router.push({
    name: 'player',
    params: { library: library.value, id: id.value },
    ...(fromStart ? { query: { start: '0' } } : {}),
  })
}

function openSeason(season: number | null) {
  void router.push({
    name: 'season',
    params: { library: library.value, id: id.value, season: seasonSegment(season) },
  })
}

function openItem(child: string) {
  void router.push({ name: 'detail', params: { library: library.value, id: child } })
}

/// One press for a season somebody has already watched elsewhere — the
/// alternative is thirteen presses. WHICH episodes are in it is decided here,
/// because the season a viewer sees can be a projection of absolute numbering
/// and the hub would have to guess.
function markSeason(season: number | null, played: boolean) {
  const inSeason = episodes.value.filter((e) => seasonOf(e, projected.value) === season)
  void mark(
    item.value!.id,
    played,
    inSeason.map((e) => e.id),
  )
}
</script>

<template>
  <!-- There is no page without the item, so this one takes the screen — and it
       offers somewhere to go, because a page you can only leave by editing the
       URL is a dead end. -->
  <!-- Only when there is nothing to show. A background re-ask can fail while
       the page is perfectly good — a tick invalidates this query, so a blip
       there would have replaced a page the viewer was reading, over a write
       that had landed. -->
  <Failed
    v-if="query.isError.value && !item"
    what="Could not load this item."
    :message="sentence(query.error.value)"
    away="Back to library"
    @retry="query.refetch()"
    @away="router.push({ name: 'library', params: { library } })"
  />

  <main v-else-if="item">
    <Btn ghost small class="mb-4" @click="goUp">{{ up.label }}</Btn>

    <DetailHead :item="item" :subline="subline" :progress="item.kind === 'show' ? null : undefined">
      <template v-if="item.kind === 'show'">
        <!-- The series' one action: get on with it. Named, so it is obvious
             which episode pressing it starts — and numbered the way the list
             below is, because reading the native fields here put "Continue ·
             E10" above a row reading "S01E10". -->
        <template v-if="next">
          <Btn @click="openItem(next.id)">
            ▶ Continue ·
            {{ seLabel(seasonOf(next, projected), episodeOf(next, projected), next.episode_end) }}
          </Btn>
          <span class="text-[13px] text-dim">{{ next.title }}</span>
        </template>
      </template>

      <!-- A record's two actions. Both are queue operations, and they are
           different questions: Play replaces what is playing, Add does not
           disturb it. Neither is offered while the track list is still coming,
           because both of them ARE the track list. -->
      <template v-else-if="item.kind === 'album'">
        <!-- A disabled control must SAY why. Absent data and an empty record
             are different reasons, and neither of them is "no". -->
        <Btn :disabled="!tracks.length" :title="whyNoTracks" @click="queue.playAlbum(tracks)">
          ▶ Play
        </Btn>
        <Btn
          ghost
          :disabled="!tracks.length"
          :title="whyNoTracks"
          @click="queue.appendAlbum(tracks)"
        >
          Add to queue
        </Btn>
        <span v-if="whyNoTracks" class="text-dim">{{ whyNoTracks }}</span>
      </template>

      <template v-else>
        <Btn
          :disabled="!best?.available"
          :title="best?.available ? undefined : 'The machine holding this file is not answering'"
          @click="play()"
        >
          ▶ {{ resumeAt ? 'Resume' : 'Play' }}
        </Btn>
        <Btn v-if="resumeAt > 0" ghost @click="play(true)">Play from start</Btn>
        <Btn
          ghost
          small
          :disabled="busy.has(item.id)"
          :title="item.played ? 'Mark as unwatched' : 'Mark as watched without playing it'"
          @click="mark(item.id, !item.played)"
        >
          <Icon name="check" />
          {{ item.played ? 'Watched' : 'Mark watched' }}
        </Btn>
      </template>
    </DetailHead>

    <!-- A series -->
    <template v-if="item.kind === 'show'">
      <div v-if="children.isError.value" class="flex items-center gap-3">
        <p class="m-0 text-warn" role="alert">
          Could not load the episodes: {{ sentence(children.error.value) }}
        </p>
        <Btn ghost small @click="children.refetch()">Try again</Btn>
      </div>
      <p v-else-if="children.data.value && children.data.value.length === 0" class="text-dim">
        No episodes in this series yet.
      </p>

      <section v-for="season in seasons" :key="String(season)" class="mt-6">
        <div class="mb-2 flex items-baseline gap-3">
          <!-- The heading is the way into the season's own page, where the
               episodes are stills rather than rows. -->
          <h2 class="m-0">
            <button
              class="flex cursor-pointer items-baseline gap-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase hover:text-teal"
              type="button"
              @click="openSeason(season)"
            >
              {{ seasonLabel(season, projected) }}
              <span class="text-dimmer" aria-hidden="true">→</span>
            </button>
          </h2>
          <span class="font-mono text-[12px] text-dimmer">
            {{ episodes.filter((e) => seasonOf(e, projected) === season && e.played).length }}/{{
              episodes.filter((e) => seasonOf(e, projected) === season).length
            }}
            watched
          </span>
          <button
            class="cursor-pointer text-[12px] text-dim underline hover:text-text"
            type="button"
            @click="
              markSeason(
                season,
                !episodes.filter((e) => seasonOf(e, projected) === season).every((e) => e.played),
              )
            "
          >
            {{
              episodes.filter((e) => seasonOf(e, projected) === season).every((e) => e.played)
                ? 'Mark season unwatched'
                : 'Mark season watched'
            }}
          </button>
        </div>

        <ul class="flex flex-col">
          <li
            v-for="episode in episodes.filter((e) => seasonOf(e, projected) === season)"
            :key="episode.id"
            class="flex items-center gap-2 border-b border-hairline last:border-0"
          >
            <button
              class="flex flex-1 cursor-pointer items-center gap-3 py-2 text-left hover:text-teal"
              type="button"
              @click="openItem(episode.id)"
            >
              <span class="w-16 shrink-0 font-mono text-[12px] text-dim">
                {{
                  seLabel(
                    seasonOf(episode, projected),
                    episodeOf(episode, projected),
                    episode.episode_end,
                  )
                }}
                <!-- Under a projection the file's own number is still worth
                     showing: it is what the filename says. -->
                <span v-if="projected" class="text-dimmer">#{{ episode.episode }}</span>
              </span>
              <span class="flex-1 truncate">{{ episode.title }}</span>
              <span v-if="episode.id === next?.id && !episode.played" class="chip-sand">
                next up
              </span>
              <span class="w-16 shrink-0 text-right font-mono text-[12px] text-dim">
                {{
                  !episode.played && episode.resume_position_ms && episode.resume_duration_ms
                    ? `${Math.round(watchedPct(episode) ?? 0)}% in`
                    : ''
                }}
              </span>
            </button>
            <!-- A sibling of the row's own button rather than inside it: a
                 button within a button is invalid, and a click that both
                 ticked the episode and opened it would be neither. -->
            <button
              class="shrink-0 cursor-pointer p-2"
              :class="episode.played ? 'text-teal' : 'text-dimmer hover:text-dim'"
              :disabled="busy.has(episode.id)"
              :title="episode.played ? 'Mark as unwatched' : 'Mark as watched'"
              :aria-label="`${episode.played ? 'Mark as unwatched' : 'Mark as watched'}: ${episode.title}`"
              type="button"
              @click="mark(episode.id, !episode.played)"
            >
              <Icon name="check" :size="15" />
            </button>
          </li>
        </ul>
      </section>
    </template>

    <!-- A record's track list. Pressing a track plays the RECORD from there,
         rather than that track alone: the numbered list is the record, and
         somebody pressing track 4 of nine means "start here". Adding one track
         on its own is the other button, and it levels by its own gain. -->
    <template v-else-if="item.kind === 'album'">
      <h2 class="mb-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">Tracks</h2>
      <div v-if="children.isError.value" class="flex items-center gap-3">
        <p class="m-0 text-warn" role="alert">
          Could not load the track list: {{ sentence(children.error.value) }}
        </p>
        <Btn ghost small @click="children.refetch()">Try again</Btn>
      </div>
      <p v-else-if="children.data.value?.length === 0" class="text-dim">
        No tracks in this record.
      </p>
      <ul v-else class="flex flex-col">
        <li
          v-for="(track, at) in tracks"
          :key="track.id"
          class="flex items-center gap-3 border-b border-hairline last:border-0"
          :class="track.id === nowPlaying && 'text-teal'"
        >
          <button
            class="flex flex-1 cursor-pointer items-center gap-3 py-1.5 text-left hover:text-teal"
            type="button"
            :aria-current="track.id === nowPlaying ? 'true' : undefined"
            :title="`Play this record from ${track.title}`"
            @click="queue.playAlbum(tracks, at)"
          >
            <!-- The playing row is marked rather than numbered: which one it is
                 matters more than where it sits. -->
            <span class="w-8 shrink-0 text-right font-mono text-[12px] text-dim">
              {{ track.id === nowPlaying ? '▶' : (track.episode ?? at + 1) }}
            </span>
            <span class="flex-1 truncate">{{ track.title }}</span>
          </button>
          <span v-if="track.played" class="flex text-teal" title="played">
            <Icon name="check" :size="13" />
          </span>
          <button
            class="cursor-pointer px-2 py-1.5 font-mono text-[11px] text-dim hover:text-teal"
            type="button"
            :aria-label="`Add ${track.title} to the queue`"
            title="Add to the queue"
            @click="queue.appendTrack(track)"
          >
            +
          </button>
        </li>
      </ul>
    </template>

    <!-- A film or an episode -->
    <template v-else>
      <section v-if="item.metadata?.cast?.length" class="mt-8">
        <h2 class="mb-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">Cast</h2>
        <ul class="flex flex-wrap gap-x-6 gap-y-1">
          <li v-for="person in item.metadata.cast" :key="person.name">
            {{ person.name }}
            <span v-if="person.character" class="text-dim">as {{ person.character }}</span>
          </li>
        </ul>
      </section>

      <!-- What the hub says it would do with this file, for this client. Never
           re-derived here: the point of asking the item what it would serve is
           that the answer comes from the code that will serve it. -->
      <section v-if="item.negotiated" class="mt-8">
        <h2 class="mb-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">
          Playback plan
        </h2>
        <div class="rounded-md border border-line bg-surface p-3">
          <div class="mb-2 flex items-center gap-3">
            <span
              class="rounded px-2 py-0.5 font-mono text-[12px] font-[650]"
              :class="`chip-${deliveryPlan(item.negotiated.cost).tone}`"
            >
              {{ deliveryPlan(item.negotiated.cost).chip }}
            </span>
            <span v-if="deliveryPlan(item.negotiated.cost).note" class="text-[13px] text-dim">
              {{ deliveryPlan(item.negotiated.cost).note }}
            </span>
            <!-- A mask left on can never be mistaken for a bug, which is the
                 whole trap this affordance would otherwise set. The panel that
                 SETS one comes with the player; the profile above already
                 reads it, so the badge cannot wait for that. -->
            <span
              v-if="masked.length"
              class="ml-auto rounded bg-warn/15 px-2 py-0.5 font-mono text-[11px] text-warn"
              title="A mask is active — this is not what your real browser would get"
            >
              masked
            </span>
          </div>
          <!-- One row per elementary stream, because that is the grain
               negotiation decides at: a file can copy its video and re-encode
               only its audio. -->
          <div
            v-for="row in [
              { label: 'video', verdict: item.negotiated.streams.video },
              { label: 'audio', verdict: item.negotiated.streams.audio },
            ]"
            :key="row.label"
            class="grid grid-cols-[80px_1fr] gap-2 font-mono text-[12px]"
          >
            <span class="text-dim">{{ row.label }}</span>
            <span v-if="row.verdict">
              <span :class="`text-${planRow(row.verdict).tone}`">
                {{ planRow(row.verdict).action }}
              </span>
              <span v-if="planRow(row.verdict).why" class="text-dim">
                — {{ planRow(row.verdict).why }}
              </span>
            </span>
          </div>
          <!-- OPS-10, and only for an admin: what the hub recorded about the
               last session for this item, whoever played it. -->
          <div v-if="me.admin" class="mt-2 border-t border-hairline pt-2">
            <Btn ghost small @click="itemLog">Last session log</Btn>
          </div>
        </div>
      </section>

      <!-- UI-27: grouped into works, so a film in seven numbered parts does
           not read as seven alternative encodes. -->
      <section v-if="works.length" class="mt-8">
        <h2 class="mb-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">
          {{ works.length === 1 ? 'Source' : 'Sources' }}
        </h2>
        <ul class="flex flex-col gap-2">
          <li
            v-for="work in works"
            :key="work.id"
            class="rounded-md border border-line bg-surface p-2"
          >
            <div v-if="work.parts.length > 1" class="mb-1 font-mono text-[11px] text-dim">
              {{ work.parts.length }} parts
              <span v-if="!work.whole" class="text-warn">
                — incomplete, {{ work.parts[0]!.parts }} expected
              </span>
            </div>
            <div
              v-for="source in work.parts"
              :key="source.path_rel"
              class="truncate font-mono text-[12px] text-dim"
            >
              {{ source.path_rel }}
            </div>
            <div class="mt-1 flex flex-wrap gap-1.5">
              <span v-if="work.parts[0]!.streams?.container" class="chip">
                {{ work.parts[0]!.streams!.container }}
              </span>
              <span
                v-for="(video, at) in work.parts[0]!.streams?.video ?? []"
                :key="`v${at}`"
                class="chip"
              >
                {{ video.codec }} {{ video.height ? `${video.height}p` : '' }}
              </span>
              <span
                v-for="(audio, at) in work.parts[0]!.streams?.audio ?? []"
                :key="`a${at}`"
                class="chip"
              >
                {{ audio.codec }}{{ audio.language ? ` ${audio.language}` : '' }}
              </span>
              <!-- One chip for the subtitles, not one per track: a file with 26
                   embedded tracks produced 26 chips all reading "text". -->
              <span
                v-if="work.parts[0]!.streams?.subtitles?.length"
                class="chip text-dim"
                :title="subtitleChipTitle(work.parts[0]!.streams!.subtitles!)"
              >
                {{ subtitleChip(work.parts[0]!.streams!.subtitles!) }}
              </span>
              <!-- Summed across the parts: one number for the work. -->
              <span class="chip text-dim">
                {{ size(work.parts.reduce((all, part) => all + part.size, 0)) }}
              </span>
              <!-- A corrected release: v2, REPACK, PROPER. Two files of the
                   same work otherwise look like the same file twice. -->
              <span v-if="work.parts[0]!.revision > 1" class="chip text-sand">
                v{{ work.parts[0]!.revision }}
              </span>
              <span v-if="work.parts.some((part) => !part.available)" class="chip text-warn">
                offline
              </span>
            </div>
          </li>
        </ul>
      </section>
      <!-- Everything this item is connected to that the providers named:
           sequels, the film a series follows, the show an episode belongs to.
           A row that is in the library is a way there; one that is not says
           so rather than offering a link to nothing. -->
      <section v-if="item.related?.length" class="mt-8">
        <h2 class="mb-2 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">Related</h2>
        <ul class="flex flex-col gap-1">
          <li v-for="link in item.related" :key="`${link.kind}-${link.title}`">
            <span class="chip text-dim">{{ link.kind.replace('_', ' ') }}</span>
            <button
              v-if="link.item_id"
              class="ml-2 cursor-pointer underline hover:text-teal"
              type="button"
              @click="openItem(link.item_id)"
            >
              {{ link.title ?? '?' }}
            </button>
            <span v-else class="ml-2 text-dim">{{ link.title ?? '?' }} (not in library)</span>
          </li>
        </ul>
      </section>
    </template>

    <Attribution :provider="item.metadata?.provider" />
  </main>
</template>

<style scoped>
@reference '../theme.css';

.chip {
  @apply rounded border border-line px-1.5 py-0.5 font-mono text-[11px];
}
.chip-sand {
  @apply rounded bg-sand/15 px-1.5 py-0.5 font-mono text-[11px] text-sand;
}
.chip-teal {
  @apply bg-teal/15 text-teal;
}
.chip-warn {
  @apply bg-warn/15 text-warn;
}
</style>
