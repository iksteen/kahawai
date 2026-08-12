import Icon from './icons'

/// A load that did not work, with a way out of it.
///
/// The screens that fetch something used to render the error string and
/// stop: no retry, no navigation, nothing but a sentence and the header.
/// A page that can only be left by editing the URL is a dead end, and the
/// most common cause — the hub restarted, the wifi blinked — is fixed by
/// asking again.
///
/// The message is kept, in mono and dimmed. It is usually the hub's own
/// words and occasionally the only clue anybody gets; hiding it behind
/// "something went wrong" would be tidier and worse.
export default function Failed({
  what,
  message,
  onRetry,
  away,
}: {
  /// What could not be loaded, in the viewer's terms.
  what: string
  message: string
  onRetry: () => void
  /// Somewhere else to go, named by the caller: the library you came from
  /// is a better offer than "home" when that is where you came from.
  /// Omitted where there is nowhere to go — the home screen's own failure.
  away?: { label: string; go: () => void }
}) {
  return (
    <div className="failed">
      <span className="failed-glyph">
        <Icon name="alert" size={22} />
      </span>
      <h2>{what}</h2>
      <p className="dim mono failed-why">{message}</p>
      <span className="failed-do">
        <button className="btn" onClick={onRetry}>
          Try again
        </button>
        {away && (
          <button className="btn ghost" onClick={away.go}>
            {away.label}
          </button>
        )}
      </span>
    </div>
  )
}
