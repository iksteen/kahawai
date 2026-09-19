import type { ResolvedDescription } from '../api/generated/model/resolvedDescription.ts'

export function descriptionProviders(metadata: ResolvedDescription): string[] {
  return [...new Set(Object.values(metadata.providers ?? {}))]
}

export function itemProviders(
  items: Iterable<{ attribution?: readonly string[] } | null | undefined>,
): string[] {
  return [...new Set(Array.from(items).flatMap((item) => item?.attribution ?? []))]
}
