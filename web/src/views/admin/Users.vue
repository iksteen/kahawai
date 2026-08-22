<script setup lang="ts">
/// HUB-10/26: accounts, and what each may see.
///
/// A chip writes the account's WHOLE access, not a change to it — that is what
/// the endpoint takes, and it is why two admins with the panel open cannot
/// interleave into a set neither picked. The write machinery is `useGrants`.
import { computed, ref } from 'vue'

import Armed from '../../components/Armed.vue'
import Btn from '../../components/Btn.vue'
import type { LibraryOverview } from '../../api/generated/model/libraryOverview.ts'
import type { UserAccess } from '../../api/generated/model/userAccess.ts'
import { adminCreateUser, adminDeleteUser, adminSetUserAdmin } from '../../api/generated/kahawai.ts'
import {
  canCreate,
  demotesSelf,
  longEnough,
  MIN_PASSWORD,
  seesEverything,
} from '../../domain/admin.ts'
import { notify } from '../../composables/notices.ts'
import { refreshTokens, whoAmI } from '../../api/session.ts'
import { useGrants } from '../../composables/grants.ts'

const props = defineProps<{
  users: UserAccess[]
  libraries: LibraryOverview[]
  broken: readonly string[]
  act: (what: () => Promise<unknown>) => Promise<boolean>
  /// Read the users again and resolve when the answer has landed. The grant
  /// writes need this, and they need to be able to WAIT for it.
  reread: () => Promise<void>
  refused: (why: string) => void
}>()

const me = computed(() => whoAmI().username)

const grants = useGrants({
  refused: (why) => props.refused(why),
  reread: () => props.reread(),
})

function toggleLibrary(user: UserAccess, library: string) {
  const row = grants.asShown(user)
  const next = row.libraries.includes(library)
    ? row.libraries.filter((l) => l !== library)
    : [...row.libraries, library]
  void grants.set(row, row.all_libraries, next)
}

function toggleAll(user: UserAccess) {
  const row = grants.asShown(user)
  void grants.set(row, !row.all_libraries, row.libraries)
}

/// The hub owns both refusals — your own rights, and the last admin. The client
/// disables the one it can see coming and reports the other.
async function setRole(user: UserAccess, isAdmin: boolean) {
  const ok = await props.act(() => adminSetUserAdmin(user.id, { admin: isAdmin }))
  if (!ok || !demotesSelf(user, isAdmin, me.value)) return
  // The write invalidated the token that authorised it. Rotate to a current
  // non-admin token before leaving: a bare reload would have bootstrap see only
  // the invalid old one and show sign-in, despite the refresh family still
  // being live.
  if (!(await refreshTokens())) {
    notify('Your role changed, but the session could not be refreshed. Sign in again.')
    return
  }
  window.location.assign('/app/')
}

const newUser = ref({ username: '', password: '', admin: false })
async function create() {
  const { username, password, admin: asAdmin } = newUser.value
  const ok = await props.act(() =>
    adminCreateUser({ username: username.trim(), password, admin: asAdmin }),
  )
  if (!ok) return
  newUser.value = { username: '', password: '', admin: false }
  notify(`Created ${username.trim()} — it can see every library until you say otherwise.`)
}

async function remove(user: UserAccess) {
  await props.act(() => adminDeleteUser(user.id))
}

/// Says so before Create is pressed, rather than after. By code point: a
/// passphrase of emoji is not half as long as `.length` claims.
const tooShort = computed(
  () => newUser.value.password !== '' && !longEnough(newUser.value.password),
)

const since = (seconds: number) =>
  new Date(seconds * 1000).toLocaleDateString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
  })

/// An account that can sign in and has nothing to look at. Not a refusal —
/// somebody meant to grant it something and did not finish.
const marooned = (user: UserAccess) =>
  !user.is_admin && !user.all_libraries && user.libraries.length === 0
</script>

<template>
  <section aria-labelledby="new-account">
    <h2 id="new-account" class="mb-3 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">
      New account
    </h2>
    <form class="mb-4 flex flex-wrap items-center gap-2" @submit.prevent="create">
      <!-- The two fields share whatever is left after the toggle and Create,
           which take only what they need. A basis rather than a width, so they
           stay equal as the column narrows, and `min-w-0` so they can shrink
           below their placeholder text instead of pushing the buttons off the
           end — at an intrinsic width the row stopped short of the panel and
           read as something half-drawn. -->
      <label class="sr-only" for="new-user">New username</label>
      <input
        id="new-user"
        v-model="newUser.username"
        class="min-w-0 flex-[1_1_120px] rounded border border-line bg-bg px-2 py-1"
        placeholder="new username"
      />
      <label class="sr-only" for="new-pass">Password</label>
      <input
        id="new-pass"
        v-model="newUser.password"
        class="min-w-0 flex-[1_1_120px] rounded border bg-bg px-2 py-1"
        :class="tooShort ? 'border-warn' : 'border-line'"
        type="password"
        autocomplete="new-password"
        :minlength="MIN_PASSWORD"
        :aria-invalid="tooShort"
        aria-describedby="pass-rule"
        :placeholder="`At least ${MIN_PASSWORD} characters`"
      />
      <!-- Said in text, not only in the placeholder: the placeholder is gone
           after the first keystroke, which is exactly when the rule starts to
           matter, and a Create button that is disabled for no stated reason is
           the whole of what was left. -->
      <span
        id="pass-rule"
        class="font-mono text-[11px]"
        :class="tooShort ? 'text-warn' : 'text-dimmer'"
      >
        at least {{ MIN_PASSWORD }} characters
      </span>
      <button
        class="cursor-pointer rounded border px-2 py-1 font-mono text-[12px]"
        :class="newUser.admin ? 'border-teal text-teal' : 'border-line text-dim'"
        type="button"
        :aria-pressed="newUser.admin"
        title="Create as an administrator"
        @click="newUser.admin = !newUser.admin"
      >
        admin
      </button>
      <Btn submit small :disabled="!canCreate(newUser.username, newUser.password)">Create</Btn>
    </form>
  </section>

  <section aria-labelledby="accounts">
    <h2
      id="accounts"
      class="mt-[22px] mb-3 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase"
    >
      Accounts
    </h2>
    <p v-if="props.broken.includes('users')" class="text-warn">
      The accounts could not be read, so this is not saying there are none.
    </p>
    <ul class="flex flex-col gap-2">
      <li
        v-for="user in props.users"
        :key="user.id"
        class="rounded border border-line bg-surface p-2"
      >
        <div class="flex flex-wrap items-center gap-3">
          <span class="font-[650]">{{ user.username }}</span>
          <button
            class="cursor-pointer rounded border px-2 py-0.5 font-mono text-[12px]"
            :class="user.is_admin ? 'border-teal text-teal' : 'border-line text-dim'"
            type="button"
            :aria-pressed="user.is_admin"
            :title="
              user.is_admin
                ? user.username === me
                  ? 'Demote this account and return to the home screen'
                  : 'Demote to an ordinary account, bound by its grants'
                : 'Make an administrator: every library, and this panel'
            "
            @click="setRole(user, !user.is_admin)"
          >
            admin
          </button>
          <!-- For an admin the same control, held on: an admin does have every
               library, and saying so with everyone else's toggle beats a
               sentence explaining why there is no toggle here. -->
          <!-- `aria-disabled`, not `disabled`. A disabled button is out of the
               tab order, so the one sentence that explains why it cannot be
               pressed is in the one place a keyboard or screen-reader user can
               never reach. This one stays reachable and does nothing. -->
          <button
            class="cursor-pointer rounded border px-2 py-0.5 font-mono text-[12px] aria-disabled:cursor-default aria-disabled:opacity-60"
            :class="
              seesEverything(grants.asShown(user))
                ? 'border-teal text-teal'
                : 'border-line text-dim'
            "
            type="button"
            :aria-disabled="user.is_admin"
            :aria-pressed="seesEverything(grants.asShown(user))"
            :title="
              user.is_admin
                ? 'An admin configures the grants, so it is not bound by them'
                : 'Every library, including ones added later'
            "
            @click="user.is_admin || toggleAll(user)"
          >
            all libraries
          </button>
          <span
            v-if="marooned(grants.asShown(user))"
            class="rounded border border-warn px-1.5 py-0.5 font-mono text-[11px] text-warn"
            title="This account can sign in, but its home screen is empty"
          >
            no access
          </span>
          <span class="ml-auto font-mono text-[11px] text-dimmer">
            since {{ since(user.created_at) }}
          </span>
          <!-- The API refuses deleting the account you are signed in as, so
               saying it before the click is kinder than an error afterwards. -->
          <span v-if="user.username === me" class="font-mono text-[11px] text-dimmer">
            signed in as this account
          </span>
          <Armed
            v-else
            label="Delete"
            armed-label="Really delete?"
            :name="`Delete ${user.username}`"
            :armed-name="`Really delete ${user.username}?`"
            @confirm="remove(user)"
          />
        </div>

        <!-- The libraries it may see, underneath: seven of them inline pushed
             everything else out of line with every other row.

             Deliberately NOT disabled while a write is out. The queue orders
             the clicks and the chips move optimistically, so a second click
             during the round trip is ordered behind the first — disabling them
             swallowed it instead, and took the just-pressed button out of the
             tab order with nothing announcing why. -->
        <div v-if="!seesEverything(grants.asShown(user))" class="mt-2 flex flex-wrap gap-1">
          <span class="font-mono text-[11px] text-dimmer">granted</span>
          <button
            v-for="library in props.libraries"
            :key="library.id"
            class="cursor-pointer rounded border px-2 py-0.5 font-mono text-[12px]"
            :class="
              grants.asShown(user).libraries.includes(library.id)
                ? 'border-teal text-teal'
                : 'border-line text-dim'
            "
            type="button"
            :aria-pressed="grants.asShown(user).libraries.includes(library.id)"
            @click="toggleLibrary(user, library.id)"
          >
            {{ library.name }}
          </button>
          <span v-if="props.broken.includes('libraries')" class="font-mono text-[11px] text-warn">
            the libraries could not be read
          </span>
        </div>
      </li>
    </ul>
  </section>
</template>
