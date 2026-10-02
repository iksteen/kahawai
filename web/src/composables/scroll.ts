import { ref, watch } from 'vue'
import { useRoute, useRouter, type RouteLocationRaw, type Router } from 'vue-router'

/// An Up button remains hierarchical on a shared link. When its destination
/// is also the previous entry, use Back so the entry's scroll and browse sort
/// survive rather than replacing them with a fresh visit.
export function backTo(router: Router, target: RouteLocationRaw) {
  if (router.options.history.state.back === router.resolve(target).fullPath) router.back()
  else void router.push(target)
}

/// Sort belongs to the browse history entry: restoring a pixel offset under
/// a different ordering would put the viewer among different items. This is
/// navigation state only; catalogue pages are fetched again on return.
export function useBrowseSort(key: string, fallback: string) {
  const router = useRouter()
  const route = useRoute()
  const stored = router.options.history.state[key]
  const sort = ref(typeof stored === 'string' ? stored : fallback)
  watch(sort, (value) => {
    router.options.history.replace(route.fullPath, {
      ...router.options.history.state,
      [key]: value,
    })
  })
  return sort
}

/// A virtual grid has no scrollable height until its first page and row
/// measurement arrive. Vue Router supports a Promise from scrollBehavior:
/// https://router.vuejs.org/guide/advanced/scroll-behavior.html#delaying-the-scroll
/// Wait for the destination's rendered readiness marker, not a fixed delay.
/// One outstanding navigation owns the observer; leaving cancels it.
export function browseScroll() {
  let cancel = () => {}
  function ready(path: string): Promise<boolean> {
    cancel()
    return new Promise((resolve) => {
      const finish = (value: boolean) => {
        observer.disconnect()
        cancel = () => {}
        resolve(value)
      }
      const check = () => {
        const page = Array.from(document.querySelectorAll<HTMLElement>('[data-scroll-page]')).find(
          (page) => page.dataset.scrollPage === path,
        )
        if (page?.dataset.scrollReady === 'true') finish(true)
      }
      const observer = new MutationObserver(check)
      cancel = () => finish(false)
      observer.observe(document.body, {
        childList: true,
        subtree: true,
        attributes: true,
        attributeFilter: ['data-scroll-page', 'data-scroll-ready'],
      })
      check()
    })
  }
  return { ready, cancel: () => cancel() }
}
