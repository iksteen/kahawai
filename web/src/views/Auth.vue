<script setup lang="ts">
/// Sign in, or create the first administrator. Two modes of one form, because
/// they differ in three words and one rule.
///
/// Real `<label>` elements, which the old screen did not have: it named its
/// fields with placeholders alone, so the name vanished the moment anything
/// was typed and a screen reader had nothing to announce for the field at all.
import { computed, nextTick, onMounted, ref, useId } from 'vue'

import Btn from '../components/Btn.vue'
import { MIN_PASSWORD, passwordLongEnough } from '../domain/auth.ts'
import { browserLogin } from '../api/session.ts'
import { sentence } from '../domain/refusal.ts'
import { setup } from '../api/generated/kahawai.ts'

const props = defineProps<{
  mode: 'setup' | 'login'
  /// Why you are here, when you did not ask to be.
  note?: string
  setupAvailable?: boolean
  setupUrl?: string | undefined
}>()
const emit = defineEmits<{ done: [] }>()

const username = ref('')
const password = ref('')
const failure = ref('')
const busy = ref(false)
const created = ref(false)

const userField = useId()
const passwordField = useId()
const alertId = useId()

/// Imperatively, not the `autofocus` attribute. The HTML autofocus candidate
/// list is processed once as the document loads, and this form is always
/// inserted afterwards — it renders only once the bootstrap round trip has
/// answered, so the attribute does nothing.
const first = ref<HTMLInputElement | null>(null)
onMounted(() => first.value?.focus())

/// The paragraph that replaces the form once the account exists.
const done = ref<HTMLElement | null>(null)

/// Off while the password is too short for the hub to accept, and never for
/// any other reason: a disabled button with no visible cause is
/// indistinguishable from a broken one.
///
/// NOT off while the request is out. Disabling the button somebody just
/// pressed takes focus off it — the browser's focus fixup moves it to `body`,
/// so Enter no longer retries and Tab restarts at the top of the page. The
/// form says it is busy instead, and `submit` refuses a second one.
const ready = computed(() => props.mode === 'login' || passwordLongEnough(password.value))

async function submit() {
  // Two logins in flight bump the session generation twice, so the first can
  // no longer install what it gets back — and the app renders signed in with
  // no bearer.
  if (busy.value) return
  busy.value = true
  failure.value = ''
  try {
    if (props.mode === 'login') {
      await browserLogin(username.value, password.value)
      emit('done')
    } else {
      await setup(
        { username: username.value, password: password.value },
        { skipAuthRefresh: true, skipAuthorization: true },
      )
      created.value = true
      // Focus follows the replacement, or it lands on `body` and Tab starts
      // again from the top of the page.
      await nextTick()
      done.value?.focus()
    }
  } catch (cause) {
    failure.value = sentence(cause)
  } finally {
    // Always. UI-24 was a hub that accepted the connection and never answered,
    // which left this form disabled with no way to cancel — on the one screen
    // with nothing else to navigate to. The request has a deadline of its own
    // now; this is the half that gives the form back when it expires.
    busy.value = false
  }
}
</script>

<template>
  <div class="grid min-h-screen place-items-center p-6">
    <!-- A card, because this is the one screen with nothing else on it: the
         panel is what says the form is a thing to fill in rather than text
         that happens to have boxes. Not a flat 340px — on a narrow phone that
         overflows the padding it is centred in, and the first screen everybody
         meets is the wrong one to have a horizontal scrollbar. -->
    <div
      class="animate-rise flex w-[min(340px,100%)] flex-col gap-3 rounded-lg border border-line bg-surface px-7 pt-8 pb-7"
    >
      <h1 class="text-[28px] font-[650] tracking-[0.04em]">
        kahawai<span class="text-teal">~</span>
      </h1>

      <!-- Setup is reachable only from the hub's own local control plane, so
           the ordinary address cannot offer the form at all. Saying where to
           go beats a form that would be refused. -->
      <!-- `role="status"`, because both of these REPLACE the form — including
           the button that was just pressed. Without it, activating "Create
           admin account" is followed by silence and a caret back at the top of
           the document. -->
      <p
        v-if="mode === 'setup' && !setupAvailable"
        class="text-[13.5px] text-dim"
        role="status"
        tabindex="-1"
      >
        Initial setup is available only through the hub's local control plane. Open
        <code class="font-mono text-teal">{{
          setupUrl ?? 'the local setup URL printed by the hub'
        }}</code>
        on the hub, connect to it with an SSH tunnel, or run
        <code class="font-mono text-teal">kahawai hub init-admin</code>.
      </p>

      <p v-else-if="created" ref="done" class="text-[13.5px] text-dim" role="status" tabindex="-1">
        Administrator created. Return to your normal kahawai address and sign in.
      </p>

      <form v-else class="flex flex-col gap-3" :aria-busy="busy" @submit.prevent="submit">
        <p class="mb-1.5 text-[13.5px] text-dim">
          {{
            mode === 'setup'
              ? 'First run. Create the initial administrator from this local-only page.'
              : note || 'Sign in to your library.'
          }}
        </p>

        <label class="text-[13px] text-dim" :for="userField">Username</label>
        <input
          :id="userField"
          ref="first"
          v-model="username"
          class="rounded border border-line bg-bg px-3 py-[9px] text-text"
          :aria-invalid="failure !== '' || undefined"
          :aria-describedby="failure ? alertId : undefined"
          autocomplete="username"
          name="username"
          required
        />

        <label class="text-[13px] text-dim" :for="passwordField">Password</label>
        <input
          :id="passwordField"
          v-model="password"
          class="rounded border border-line bg-bg px-3 py-[9px] text-text"
          :autocomplete="mode === 'setup' ? 'new-password' : 'current-password'"
          :aria-invalid="failure !== '' || undefined"
          :aria-describedby="
            [mode === 'setup' ? `${passwordField}-rule` : '', failure ? alertId : '']
              .filter(Boolean)
              .join(' ') || undefined
          "
          name="password"
          type="password"
          required
        />
        <p
          v-if="mode === 'setup'"
          :id="`${passwordField}-rule`"
          class="mb-1.5 text-[13.5px] text-dim"
        >
          At least {{ MIN_PASSWORD }} characters.
        </p>

        <!-- `role="alert"` so it is read when it appears — and named by both
             fields while it is showing, so somebody who tabs back into one
             hears the error rather than an unexplained invalid state. -->
        <p v-if="failure" :id="alertId" class="text-[13px] text-warn" role="alert">
          {{ failure }}
        </p>

        <Btn submit :disabled="!ready" :aria-disabled="busy" class="mt-1">
          {{ busy ? 'Working…' : mode === 'setup' ? 'Create admin account' : 'Sign in' }}
        </Btn>
      </form>
    </div>
  </div>
</template>
