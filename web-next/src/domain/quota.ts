/// HUB-21/24: what is left of the subtitle download entitlement.
///
/// Said out loud wherever a download can be started, because the anonymous
/// budget is SHARED by everyone using this hub — spending it is spending
/// somebody else's, and nothing else on screen would say so.

import type { Quota } from '../api/generated/model/quota.ts'

export function quotaLabel(quota: Quota | null | undefined): string {
  if (!quota || quota.remaining === null) return ''
  const scope = quota.per_account ? '' : ' — shared by everyone on this server'
  const resets =
    quota.resets_in_secs && quota.resets_in_secs > 0
      ? `, resets in ${Math.max(1, Math.round(quota.resets_in_secs / 3600))} h`
      : ''
  const total = quota.total ? ` of ${quota.total}` : ''
  return `${quota.remaining}${total} downloads left today${resets}${scope}`
}
