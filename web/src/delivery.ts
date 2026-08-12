/// One aggregate name for the work a playback plan performs. Pipeline `mode`
/// is deliberately not used here: it describes direct bytes vs a local or
/// dispatched HLS pipeline, while either HLS pipeline may copy one elementary
/// stream and encode another.
const DELIVERY = {
  direct: { chip: 'DIRECT', tone: 'teal', note: '' },
  copy: { chip: 'REMUX', tone: 'teal', note: '' },
  audio_encode: {
    chip: 'TRANSCODE',
    tone: 'sand',
    note: 'the audio is re-encoded; the picture is copied',
  },
  video_encode: { chip: 'TRANSCODE', tone: 'sand', note: '' },
  unplayable: {
    chip: 'UNPLAYABLE',
    tone: 'warn',
    note: 'nothing here can be delivered to this browser',
  },
} as const

export function deliveryPlan(cost: string) {
  return (
    DELIVERY[cost as keyof typeof DELIVERY] ?? {
      chip: cost.toUpperCase(),
      tone: 'warn' as const,
      note: '',
    }
  )
}
