import { Component, type ReactNode } from 'react'
import Failed from './Failed'

/// The last line: a screen that throws while rendering takes the whole app
/// with it, and React unmounts the tree rather than leaving a half-drawn
/// one. Without this that is a white page — no header, no way back, nothing
/// to report but "it went blank".
///
/// A class, because there is no hook form of this. `getDerivedStateFromError`
/// and `componentDidCatch` are the only way React offers to catch a render
/// throw, and both are class-only. It is the one class in the app and it is
/// not a style choice.
///
/// Note what this does NOT catch, so nobody trusts it too far: errors thrown
/// in event handlers, in promises, or in anything asynchronous. Those never
/// reach a boundary — they are the `catch` blocks' job, and the screens that
/// fetch handle them with `Failed` instead.
export default class Boundary extends Component<
  {
    children: ReactNode
    onHome: () => void
    /// Wraps the FAILURE only, never the children — so a boundary around a
    /// fixed-position thing can put its card where that thing was. The music
    /// dock is `position: fixed`, so its card landed at the end of the page
    /// flow: from the viewer's side the bar simply vanished, with the offer to
    /// retry several screens further down.
    className?: string
  },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null }

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  componentDidCatch(error: Error, info: { componentStack?: string | null }) {
    // The console is where a stack is actually readable, and this is a bug
    // rather than a condition — somebody should be able to find out which
    // component died without reproducing it twice.
    console.error('render failed', error, info.componentStack)
  }

  render() {
    const { error } = this.state
    if (!error) return this.props.children
    const card = (
      <Failed
        what="This screen stopped working."
        message={error.message || String(error)}
        // Re-render the same route from scratch. A render throw is usually a
        // shape the screen did not expect; asking again is worth one press
        // before reaching for a reload.
        onRetry={() => this.setState({ error: null })}
        away={{
          label: 'Home',
          go: () => {
            this.setState({ error: null })
            this.props.onHome()
          },
        }}
      />
    )
    const { className } = this.props
    return className ? <div className={className}>{card}</div> : card
  }
}
