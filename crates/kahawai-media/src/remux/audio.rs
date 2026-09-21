use super::*;

/// decodebin (auto-picks the best-ranked decoder — registry-derived, per
/// the fallback strategy) → audioconvert → audioresample → AAC encoder →
/// aacparse (raw→ADTS for the TS muxer) → muxer pad. The only decode/
/// encode work in the hub, and audio-only by design: a few % CPU.
/// Canonical AAC input layouts, most channels first. `channel-mask` bits
/// are GStreamer positions: 0x3f = 5.1 (FL FR FC LFE RL RR), 0xc3f adds
/// SL SR (standard side-surround 7.1 — what DTS-HD 7.1 decodes to),
/// 0xff instead adds FLC FRC (7.1 "front wide").
pub(super) const AAC_LAYOUTS: &[(u32, u64)] =
    &[(8, 0xc3f), (8, 0xff), (6, 0x3f), (2, 0x3), (1, 0x4)];

/// Can the AAC encoder carry this layout ALL THE WAY to the client —
/// measured once per layout by the real tail: encode, mux into MPEG-TS,
/// demux, decode, and require decoded audio to actually come out.
///
/// Every weaker probe was tried, and each one passed a broken path:
/// - *Does the link form?* The pad template lies (fdkaacenc advertises
///   `channels={1,2,3,4,5,6,8}` unconditionally, then refuses standard
///   side-surround 7.1 caps), and refused caps do not even fail the
///   link — negotiation fixates on the template's first value and the
///   encode silently becomes mono.
/// - *Does a decoder linked directly to the encoder work?* False pass:
///   the decoder reads the channel config from the caps' codec_data,
///   which never survives TS. In ADTS there is only a 3-bit config
///   field, fdk's 8-channel modes are not expressible in it, and the
///   stream decodes nowhere — "channel element 1.1 is not allocated"
///   on every frame, on every ffmpeg, on both fleets.
/// - *EOS-only checking.* Decode failures are per-buffer WARNINGS in
///   GStreamer; a pipeline whose every frame fails still ends in EOS.
///   Success is decoded BUFFERS ARRIVING, nothing less.
/// - *Probing the pin without the source.* A count-only pin passed when
///   audiotestsrc freely negotiated the encoder's favourite layout —
///   but the REAL chain's audioconvert prefers passthrough, handed the
///   encoder the source's side-surround caps it accepts-and-mis-signals,
///   and shipped the broken stream the probe had just blessed. The probe
///   therefore stages the source's own (channels, mask) upstream of the
///   pin, exactly like the pipeline it stands in for.
pub(super) fn aac_accepts(enc: &str, source: (u32, u64), channels: u32, mask: Option<u64>) -> bool {
    type Key = ((u32, u64), u32, Option<u64>);
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Key, bool>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(hit) = seen.lock().unwrap().get(&(source, channels, mask)) {
        return *hit;
    }
    // avdec_aac is libav — the same decoder family as ffmpeg and the
    // browsers, i.e. the strictness that actually matters. Other gst
    // decoders may share the encoder's dialect and false-pass.
    const DECODERS: &[&str] = &["avdec_aac", "fdkaacdec", "faad"];
    let dec = DECODERS
        .iter()
        .find(|d| gst::ElementFactory::find(d).is_some());
    let (sch, smask) = source;
    let src = if smask != 0 {
        format!("audio/x-raw,channels={sch},channel-mask=(bitmask)0x{smask:x}")
    } else {
        format!("audio/x-raw,channels={sch}")
    };
    let pin = match mask {
        Some(m) => format!("audio/x-raw,channels={channels},channel-mask=(bitmask)0x{m:x}"),
        None => format!("audio/x-raw,channels={channels}"),
    };
    let ok = match dec {
        Some(dec) => dry_run_yields_output(&format!(
            "audiotestsrc num-buffers=10 ! audioconvert ! {src} \
             ! audioconvert ! {pin} ! audioresample \
             ! {enc} ! aacparse ! mpegtsmux ! tsdemux ! aacparse ! {dec} \
             ! audio/x-raw,channels={channels} ! fakesink name=probesink"
        )),
        // No decoder at all: nothing can verify the bitstream, so only
        // layouts that every known fdk/libav build signals correctly in
        // ADTS are trusted (5.1 and below).
        None => {
            channels <= 6
                && dry_run(&format!(
                    "audiotestsrc num-buffers=5 ! audioconvert ! {src} \
                     ! audioconvert ! {pin} ! audioresample ! {enc} ! fakesink"
                ))
        }
    };
    tracing::debug!(
        encoder = enc,
        ?source,
        channels,
        ?mask,
        accepted = ok,
        "AAC layout probe"
    );
    seen.lock().unwrap().insert((source, channels, mask), ok);
    ok
}

/// [`dry_run`], plus the requirement that at least one buffer reaches the
/// sink named `probesink` — the difference between "the pipeline ended"
/// and "the pipeline produced anything" (see [`aac_accepts`]).
pub(super) fn dry_run_yields_output(launch: &str) -> bool {
    let Ok(p) = gst::parse::launch(launch) else {
        return false;
    };
    let Some(pipe) = p.downcast_ref::<gst::Pipeline>() else {
        return false;
    };
    let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let Some(sinkpad) = pipe.by_name("probesink").and_then(|s| s.static_pad("sink")) else {
        return false;
    };
    let c = count.clone();
    sinkpad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    if p.set_state(gst::State::Playing).is_err() {
        return false;
    }
    let eos = p
        .bus()
        .and_then(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
        })
        .is_some_and(|msg| msg.type_() == gst::MessageType::Eos);
    let _ = p.set_state(gst::State::Null);
    eos && count.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// Channel count as the phrase a viewer knows it by.
pub(super) fn layout_label(channels: u32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}

/// The source's own layout when the encoder round-trips it. Otherwise the
/// largest canonical layout it accepts below the client's ceiling;
/// `audioconvert` performs the positional matrix and the loudness analyzer
/// meters that exact target. Positioned candidates come first; count-only
/// ones remain a compatibility fallback for encoders that reject an explicit
/// mask they can nevertheless produce.
pub(super) fn aac_input_layout(
    enc: &str,
    channels: u32,
    mask: u64,
    ceiling: Option<u32>,
) -> Option<(u32, Option<u64>)> {
    let bound = ceiling
        .filter(|channels| *channels > 0)
        .map_or(channels, |ceiling| ceiling.min(channels));
    let positioned = std::iter::once((channels, Some(mask)))
        .filter(|_| mask != 0 && channels <= bound)
        .chain(
            AAC_LAYOUTS
                .iter()
                .filter(|(target_channels, target_mask)| {
                    *target_channels <= bound
                        && (*target_channels < channels
                            || mask == 0
                            || *target_mask & mask == *target_mask)
                })
                .map(|(target_channels, target_mask)| (*target_channels, Some(*target_mask))),
        );
    let count_only = std::iter::once(channels)
        .chain(AAC_LAYOUTS.iter().map(|(channels, _)| *channels))
        .filter(move |channels| *channels <= bound)
        .map(|channels| (channels, None));
    positioned
        .chain(count_only)
        .find(|(target_channels, target_mask)| {
            aac_accepts(enc, (channels, mask), *target_channels, *target_mask)
        })
}
pub(super) fn opus_output_layout(
    source: crate::loudness::AudioLayout,
    ceiling: Option<u32>,
) -> crate::loudness::AudioLayout {
    let bound = ceiling.unwrap_or(8).clamp(1, 8).min(source.channels);
    let layouts = crate::loudness::measured_layouts(source);
    if source.channel_mask == 0
        && let Some(positioned) = layouts.iter().find(|layout| {
            layout.channels == source.channels
                && layout.channel_mask != 0
                && layout.channels <= bound
        })
    {
        return *positioned;
    }
    layouts
        .into_iter()
        .find(|layout| layout.channels <= bound && layout.channel_mask != 0)
        .unwrap_or_else(|| crate::loudness::AudioLayout::new(bound, 0))
}

/// Exact layout a current local audio encoder will accept for this plan.
/// `None` means force must retain the ordinary direct/copy plan.
pub fn exact_audio_layout(
    info: &kahawai_core::media::MediaInfo,
    plan: &RemuxPlan,
) -> Option<crate::loudness::AudioLayout> {
    if plan.audio != StreamMode::Encode {
        return None;
    }
    let audio = info.audio.get(plan.audio_track)?;
    let source = crate::loudness::AudioLayout::from_stream(audio.channels, audio.layout.as_deref());
    let output = match plan.audio_codec {
        AudioTarget::Aac => {
            let encoder = aac_encoder()?;
            let (channels, mask) = aac_input_layout(
                encoder,
                source.channels,
                source.channel_mask,
                plan.max_channels,
            )?;
            crate::loudness::AudioLayout::new(channels, mask?)
        }
        AudioTarget::Opus => opus_output_layout(source, plan.max_channels),
    };
    crate::loudness::resolved_measured_layouts(source, source)
        .contains(&output)
        .then_some(output)
}

#[derive(Clone, Copy)]
pub(super) struct AudioLoudnessGains {
    pub(super) exact: crate::loudness::AudioLayoutGains,
    pub(super) stereo_db: Option<f64>,
    pub(super) native_db: Option<f64>,
    pub(super) source_channels: Option<u32>,
}

impl AudioLoudnessGains {
    pub(super) fn for_layout(&self, layout: crate::loudness::AudioLayout) -> Option<f64> {
        if self.exact.iter().any(Option::is_some) {
            return self
                .exact
                .iter()
                .flatten()
                .find(|gain| gain.layout == layout)
                .map(|gain| gain.gain_db)
                .filter(|value| value.is_finite());
        }
        let candidate = if layout.channels == 2 {
            self.stereo_db
        } else if self.source_channels == Some(layout.channels) {
            self.native_db
        } else {
            None
        };
        candidate.filter(|value| value.is_finite())
    }
}

/// Pin the encoder's input layout from the decoded caps, on the caps
/// event that precedes the first buffer — i.e. before the encoder
/// negotiates, which is the whole point (see [`aac_accepts`]).
pub(super) fn install_layout_pin(
    pad: &gst::Pad,
    filter: &gst::Element,
    ceiling: Option<u32>,
    enc: &str,
    facts_dir: &std::path::Path,
) {
    let filter = filter.clone();
    let enc = enc.to_string();
    let facts_dir = facts_dir.to_path_buf();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(ev)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(c) = ev.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(s) = c.caps().structure(0) else {
            return gst::PadProbeReturn::Remove;
        };
        let channels: u32 = s.get::<i32>("channels").unwrap_or(0).max(0) as u32;
        let mask = s
            .get::<gst::Bitmask>("channel-mask")
            .map(|b| *b)
            .unwrap_or(0);
        if channels == 0 {
            return gst::PadProbeReturn::Remove;
        }
        match aac_input_layout(&enc, channels, mask, ceiling) {
            Some((n, m)) => {
                let mut b = gst::Caps::builder("audio/x-raw").field("channels", n as i32);
                if let Some(m) = m {
                    b = b.field("channel-mask", gst::Bitmask::new(m));
                }
                filter.set_property("caps", b.build());
                tracing::info!(
                    source_channels = channels,
                    source_mask = format!("0x{mask:x}"),
                    encoded_channels = n,
                    encoded_mask = m.map(|m| format!("0x{m:x}")).unwrap_or_else(|| "any".into()),
                    encoder = %enc,
                    "AAC input layout pinned"
                );
                if n != channels {
                    crate::facts::report(
                        &facts_dir,
                        "audio",
                        format!("{} → {}", layout_label(channels), layout_label(n)),
                    );
                }
            }
            None => {
                tracing::warn!(
                    channels,
                    mask = format!("0x{mask:x}"),
                    encoder = %enc,
                    "no AAC input layout accepted; leaving negotiation to the encoder"
                );
                crate::facts::report(
                    &facts_dir,
                    "audio",
                    format!("{} has no encodable layout", layout_label(channels)),
                );
            }
        }
        gst::PadProbeReturn::Remove
    });
}

pub(super) fn install_opus_layout_pin(
    pad: &gst::Pad,
    filter: &gst::Element,
    ceiling: Option<u32>,
    facts_dir: &std::path::Path,
) {
    let filter = filter.clone();
    let facts_dir = facts_dir.to_path_buf();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(source) = crate::loudness::layout_from_caps(caps.caps()) else {
            return gst::PadProbeReturn::Remove;
        };
        let target = opus_output_layout(source, ceiling);
        let mut caps = gst::Caps::builder("audio/x-raw").field("channels", target.channels as i32);
        if target.channel_mask != 0 {
            caps = caps.field("channel-mask", gst::Bitmask::new(target.channel_mask));
        }
        filter.set_property("caps", caps.build());
        if target.channels != source.channels {
            crate::facts::report(
                &facts_dir,
                "audio",
                format!(
                    "{} → {}",
                    layout_label(source.channels),
                    layout_label(target.channels)
                ),
            );
        }
        gst::PadProbeReturn::Remove
    });
}

pub(super) fn set_loudness_volume(gain: &gst::Element, gain_db: Option<f64>) {
    gain.set_property(
        "volume-full-range",
        gain_db.map_or(1.0, crate::loudness::gain_multiplier),
    );
}

pub(super) fn install_loudness_gain_pin(
    pad: &gst::Pad,
    gain: &gst::Element,
    loudness: AudioLoudnessGains,
    facts_dir: &std::path::Path,
) {
    let gain = gain.clone();
    let facts_dir = facts_dir.to_path_buf();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(layout) = crate::loudness::layout_from_caps(caps.caps()) else {
            return gst::PadProbeReturn::Remove;
        };
        let applied = loudness.for_layout(layout);
        set_loudness_volume(&gain, applied);
        if let Some(db) = applied {
            tracing::info!(
                gain_db = db,
                channels = layout.channels,
                mask = format!("0x{:x}", layout.channel_mask),
                "audio loudness gain applied"
            );
            crate::facts::report(
                facts_dir.as_path(),
                "audio",
                format!("loudness {db:+.2} dB"),
            );
        }
        gst::PadProbeReturn::Remove
    });
}

#[allow(clippy::too_many_arguments)] // one plan, spelled out
pub(super) fn build_audio_encode_chain(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    caps_name: &str,
    gate: &Option<Arc<SeekGate>>,
    target: AudioTarget,
    max_channels: Option<u32>,
    loudness: AudioLoudnessGains,
    facts_dir: &std::path::Path,
) {
    let encoder = match target {
        AudioTarget::Aac => aac_encoder(),
        AudioTarget::Opus => opus_encoder(),
    };
    let Some(enc_name) = encoder else {
        // Planner guarantees this; guard anyway (fakesink beats a stall).
        tracing::error!(
            target = target.as_str(),
            "audio encode routed with no verified encoder"
        );
        return;
    };
    // The AC-3 family's caps cannot be trusted: ac3parse labels E-AC-3
    // dependent-substream tracks (DD+ 7.1) as plain AC-3, and decodebin
    // then plugs a52dec, which dies on every block (corpus finding:
    // Despicable Me 3 / Super Mario). libav's eac3 decoder handles both
    // syntaxes, so for ac3/eac3 caps force it via a caps rewrite instead
    // of trusting autoplug. Availability-guarded; decodebin otherwise.
    let ac3_family = matches!(caps_name, "audio/x-ac3" | "audio/x-eac3");
    if ac3_family && gst::ElementFactory::find("avdec_eac3").is_some() {
        let setter = gst::ElementFactory::make("capssetter")
            .property("caps", gst::Caps::new_empty_simple("audio/x-eac3"))
            .property("join", false)
            .property("replace", true)
            .build()
            .unwrap();
        let dec = gst::ElementFactory::make("avdec_eac3").build().unwrap();
        build_audio_tail(
            pipe,
            from,
            sinkpad,
            enc_name,
            target,
            &[setter, dec],
            gate,
            max_channels,
            loudness,
            facts_dir,
        );
        return;
    }
    let decode = gst::ElementFactory::make("decodebin").build().unwrap();
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    // audioconvert does the remap; this capsfilter says what to remap TO,
    // filled in from the decoded caps once they are known (the HUB-15
    // client ceiling is one bound on that choice, the encoder's own
    // accepted layouts the other).
    let limiter = gst::ElementFactory::make("capsfilter").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let gain = gst::ElementFactory::make("volume").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    // 192000: bit/s on fdkaacenc/avenc_aac AND opusenc — same string.
    set_prop_str_if_present(&enc, "bitrate", "192000");
    // AAC rides through aacparse (ADTS/raw negotiation with the muxer);
    // opusenc's output needs no parser.
    let parse: Option<gst::Element> = match target {
        AudioTarget::Aac => Some(gst::ElementFactory::make("aacparse").build().unwrap()),
        AudioTarget::Opus => {
            // The layout-pin machinery below exists because fdkaacenc
            // lies about 7.1; opusenc does not — its ceiling is a plain
            // caps bound (channel-mapping-family covers ≤8ch).
            opus_limit(&limiter, max_channels);
            None
        }
    };
    install_loudness_gain_pin(
        &gain.static_pad("sink").unwrap(),
        &gain,
        loudness,
        facts_dir,
    );

    let mut chain: Vec<&gst::Element> = vec![&convert, &limiter, &resample, &gain, &enc];
    chain.extend(parse.iter());
    pipe.add(&decode).unwrap();
    pipe.add_many(chain.iter().copied()).unwrap();
    if !link_and_start(&chain, Some(&decode)) {
        return;
    }
    let out = chain.last().unwrap().static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "remux: encode chain → muxer link failed");
    }
    let convert_sink = convert.static_pad("sink").unwrap();
    let pin_target = limiter.clone();
    let pin_enc = enc_name.to_string();
    let pin_dir = facts_dir.to_path_buf();
    decode.connect_pad_added(move |_, pad| {
        if convert_sink.is_linked() {
            return; // first decoded stream wins
        }
        if target == AudioTarget::Aac {
            install_layout_pin(pad, &pin_target, max_channels, &pin_enc, &pin_dir);
        } else {
            install_opus_layout_pin(pad, &pin_target, max_channels, &pin_dir);
        }
        if let Err(e) = pad.link(&convert_sink) {
            tracing::warn!(error = %e, "remux: decodebin → encode chain link failed");
        }
    });
    if let Err(e) = from.link(&decode.static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "remux: → decodebin link failed");
    }
}

/// Opus channel bound: a plain caps range up to min(ceiling, 8) —
/// opusenc handles multichannel natively (channel-mapping-family 1),
/// so no accept-probe is needed, just the client's ceiling.
pub(super) fn opus_limit(limiter: &gst::Element, max_channels: Option<u32>) {
    let max = max_channels.unwrap_or(8).min(8) as i32;
    limiter.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("channels", gst::IntRange::new(1, max))
            .build(),
    );
}

/// Static front-end variant of the audio encode chain: `from` →
/// front elements → audioconvert → audioresample → encoder → aacparse →
/// muxer pad. Used when the decoder must be chosen explicitly instead of
/// trusting decodebin's caps-based autoplug.
#[allow(clippy::too_many_arguments)] // internal fan-out point: one call site
pub(super) fn build_audio_tail(
    pipe: &gst::Pipeline,
    from: &gst::Pad,
    sinkpad: gst::Pad,
    enc_name: &str,
    target: AudioTarget,
    front: &[gst::Element],
    gate: &Option<Arc<SeekGate>>,
    max_channels: Option<u32>,
    loudness: AudioLoudnessGains,
    facts_dir: &std::path::Path,
) {
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let limiter = gst::ElementFactory::make("capsfilter").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let gain = gst::ElementFactory::make("volume").build().unwrap();
    let enc = gst::ElementFactory::make(enc_name).build().unwrap();
    set_prop_str_if_present(&enc, "bitrate", "192000");
    let parse: Option<gst::Element> = match target {
        AudioTarget::Aac => Some(gst::ElementFactory::make("aacparse").build().unwrap()),
        AudioTarget::Opus => {
            opus_limit(&limiter, max_channels);
            None
        }
    };
    install_loudness_gain_pin(
        &gain.static_pad("sink").unwrap(),
        &gain,
        loudness,
        facts_dir,
    );

    // The explicit decoder's own src pad carries the decoded caps, so the
    // layout and loudness gain are pinned before the first buffer.
    if let Some(src) = front.last().and_then(|el| el.static_pad("src")) {
        if target == AudioTarget::Aac {
            install_layout_pin(&src, &limiter, max_channels, enc_name, facts_dir);
        } else {
            install_opus_layout_pin(&src, &limiter, max_channels, facts_dir);
        }
    }

    let mut chain: Vec<&gst::Element> = front.iter().collect();
    chain.extend([&convert, &limiter, &resample, &gain, &enc]);
    chain.extend(parse.iter());
    pipe.add_many(chain.iter().copied()).unwrap();
    if !link_and_start(&chain, None) {
        return;
    }
    let out = chain.last().unwrap().static_pad("src").unwrap();
    guard_pts(&out);
    if let Some(g) = gate {
        g.install(&out);
    }
    if let Err(e) = out.link(&sinkpad) {
        tracing::warn!(error = %e, "remux: encode chain → muxer link failed");
    }
    if let Err(e) = from.link(&chain[0].static_pad("sink").unwrap()) {
        tracing::warn!(error = %e, "remux: → decoder link failed");
    }
}
