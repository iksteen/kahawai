/// Save a fetched body as a file.
///
/// Fetched, not linked. Everything worth downloading here is behind the bearer
/// and a bare `<a href>` carries no Authorization header — it would save the
/// sign-in refusal instead of the log.
export function saveAs(name: string, text: string, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type }))
  const link = document.createElement('a')
  link.href = url
  link.download = name
  // IN the document, and revoked on a later turn. A detached anchor's
  // synthetic click has historically done nothing in Firefox, and revoking in
  // the same tick can pull the object out from under a download that has not
  // started reading it yet.
  document.body.append(link)
  try {
    link.click()
  } finally {
    link.remove()
    setTimeout(() => URL.revokeObjectURL(url), 0)
  }
}
