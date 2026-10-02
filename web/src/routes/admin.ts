import type { RouteRecordRaw } from 'vue-router'

/// Each admin section has its own address. The bare admin link opens the
/// first section; unknown sections are redirected there by the router.
export const adminRoutes: RouteRecordRaw[] = [
  {
    path: '/admin/:section(satellites|libraries|providers|users|sessions|work)?',
    name: 'admin',
    component: () => import('../views/Admin.vue'),
  },
  {
    path: '/admin/:pathMatch(.*)*',
    redirect: (to) => ({
      name: 'admin',
      params: { section: 'satellites' },
      query: to.query,
      hash: to.hash,
    }),
  },
]
