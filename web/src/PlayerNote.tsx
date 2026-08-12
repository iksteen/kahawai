import { useEffect, useState } from 'react'
import { NOTE_MS, onPlayerNote } from './player-note'

/// The host for notes painted inside the picture. Mounted by the player, so it
/// is inside `.videobox` and survives fullscreen — see player-note.ts.
///
/// `hidden` while a dialog owns the screen: a note behind a scrim is a message
/// nobody can act on, and the dialog is already saying the more important
/// thing.
export default function PlayerNote({ hidden }: { hidden: boolean }) {
  const [note, setNote] = useState('')
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined
    onPlayerNote((msg) => {
      clearTimeout(timer)
      setNote(msg)
      timer = setTimeout(() => setNote(''), NOTE_MS)
    })
    return () => {
      onPlayerNote(null)
      clearTimeout(timer)
    }
  }, [])
  if (!note || hidden) return null
  return (
    <div className="player-note" role="status">
      {note}
    </div>
  )
}
