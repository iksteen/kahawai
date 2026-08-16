/// Addresses.
///
/// The old app hand-rolled this, and every subtle part of it — push versus
/// replace, an `ours` marker on the history entry, resetting the scroll only
/// on a push — is something the router does natively. What it cannot decide is
/// in `domain/routes.ts`.

import { createRouter, createWebHistory } from 'vue-router'

const Placeholder = () => import('./views/Placeholder.vue')
const Home = () => import('./views/Home.vue')
const Library = () => import('./views/Library.vue')
const Detail = () => import('./views/Detail.vue')
const Season = () => import('./views/Season.vue')
const Settings = () => import('./views/Settings.vue')
const Admin = () => import('./views/Admin.vue')

export const router = createRouter({
  // The hub serves the app under /app/ and falls back to the shell for any
  // path below it, so client-side routes survive a reload and a shared link.
  history: createWebHistory('/app/'),
  routes: [
    { path: '/', name: 'libraries', component: Home },
    { path: '/admin', name: 'admin', component: Admin },
    { path: '/settings', name: 'settings', component: Settings },
    { path: '/library/:library', name: 'library', component: Library },
    {
      path: '/library/:library/item/:id',
      name: 'detail',
      component: Detail,
    },
    {
      // `all` rather than an empty segment — see `seasonSegment`.
      path: '/library/:library/item/:id/season/:season',
      name: 'season',
      component: Season,
    },
    {
      // The player's own address, so a deep link, a reload and a forward all
      // land where pressing Play does.
      path: '/library/:library/item/:id/play',
      name: 'player',
      component: Placeholder,
    },
    // Anything else is the home screen rather than a dead end: these paths are
    // typed and shared by people, not only produced by the app.
    //
    // By path, not by name: redirecting to a name carries this route's own
    // `pathMatch` param along to a route that has no such param, and the
    // router drops it with a console warning on every mistyped address — the
    // exact traffic this line exists for.
    { path: '/:pathMatch(.*)*', redirect: '/' },
  ],
  scrollBehavior(_to, _from, savedPosition) {
    // A push is a new screen and the browser does not move for one. The
    // library grid reserves the whole library's height — tens of thousands of
    // pixels — so opening an item from row 150 kept `scrollY` and clamped it
    // to the short page's maximum: you landed at the BOTTOM of the item, on
    // the sources list, with the title and Play button off-screen above.
    //
    // Back and forward restore where they were, which is `savedPosition` and
    // is what a person expects from those two buttons alone.
    return savedPosition ?? { top: 0 }
  },
})
