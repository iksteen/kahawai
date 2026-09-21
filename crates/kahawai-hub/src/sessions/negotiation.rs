use super::*;

pub(super) fn apply_audio_loudness_measurement(
    plan: &mut kahawai_media::remux::RemuxPlan,
    preference: LoudnessPreference,
    measured: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
) {
    plan.stereo_gain_db = None;
    plan.native_gain_db = None;
    plan.loudness_source_channels = None;
    plan.loudness_gains = [None; kahawai_media::loudness::MAX_LAYOUT_GAINS];
    if preference.enabled()
        && let Some(measured) = measured
    {
        for (slot, measurement) in plan.loudness_gains.iter_mut().zip(&measured.layouts) {
            *slot = Some(kahawai_media::loudness::AudioLayoutGain {
                layout: measurement.layout,
                gain_db: kahawai_media::loudness::gain_db(measurement.loudness),
            });
        }
        plan.loudness_source_channels = Some(measured.source.channels);
        plan.native_gain_db = measured
            .get(measured.source)
            .map(kahawai_media::loudness::gain_db);
        plan.stereo_gain_db = measured
            .get(kahawai_media::loudness::AudioLayout::new(2, 0x3))
            .or_else(|| {
                measured
                    .get(measured.source)
                    .filter(|_| measured.source.channels <= 2)
            })
            .map(kahawai_media::loudness::gain_db);
    }
}

pub(super) async fn fill_audio_loudness_gains(
    registry: &Registry,
    parts: &[PartSource],
    plan: &mut kahawai_media::remux::RemuxPlan,
    preference: LoudnessPreference,
    known: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
) -> Result<()> {
    let measured = if !preference.enabled()
        || plan.audio != kahawai_media::remux::StreamMode::Encode
        || parts.len() != 1
    {
        None
    } else if preference.force() {
        known
    } else {
        parts[0]
            .file_id
            .audio_loudness(
                registry,
                plan.audio_track,
                parts[0].size,
                parts[0].mtime_unix,
            )
            .await?
    };

    apply_audio_loudness_measurement(plan, preference, measured);
    Ok(())
}
pub(super) fn same_video_path(
    left: &kahawai_media::remux::RemuxPlan,
    right: &kahawai_media::remux::RemuxPlan,
) -> bool {
    left.video == right.video
        && left.video_track == right.video_track
        && left.video_kbps == right.video_kbps
        && left.max_height == right.max_height
        && left.tone_map == right.tone_map
        && left.deinterlace == right.deinterlace
        && left.burn_subtitle == right.burn_subtitle
        && left.burn_ass == right.burn_ass
        && left.video_codec == right.video_codec
        && left.segment_format == right.segment_format
}

pub(super) fn replanned_verdict(
    plan: &kahawai_media::remux::RemuxPlan,
    video_verdict: &str,
    audio_verdict: &str,
) -> Result<(String, String)> {
    anyhow::ensure!(plan.playable(), "selected track is not playable");
    Ok((video_verdict.to_owned(), audio_verdict.to_owned()))
}

pub(super) fn loudness_protocol_feature(
    plan: &kahawai_media::remux::RemuxPlan,
) -> Option<kahawai_proto::ProtocolFeature> {
    plan.loudness_gains
        .iter()
        .any(Option::is_some)
        .then_some(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains)
}

pub(super) fn wire_scalar_loudness(
    plan: &kahawai_media::remux::RemuxPlan,
) -> (Option<f64>, Option<f64>, Option<u32>) {
    // Presence is authoritative in the protocol-4 baseline. Sentinels retain
    // the worker argv's distinction between absent and an exact 0 dB value.
    (
        Some(plan.stereo_gain_db.unwrap_or(f64::NAN)),
        Some(plan.native_gain_db.unwrap_or(f64::NAN)),
        Some(plan.loudness_source_channels.unwrap_or(0)),
    )
}

pub(super) fn placement_need(
    plan: &kahawai_media::remux::RemuxPlan,
    info: &kahawai_core::media::MediaInfo,
    parts: &[PartSource],
    burns_ass: bool,
) -> (crate::registry::PlacementNeed, String) {
    use kahawai_media::remux::StreamMode;

    let class = if plan.video == StreamMode::Encode {
        let video = info.video.first();
        crate::pace::work_class(
            video.map_or(0, |video| video.height),
            video.map_or("", |video| video.codec.as_str()),
            plan.video_codec.as_str(),
            plan.tone_map,
        )
    } else {
        String::new()
    };
    let need = crate::registry::PlacementNeed {
        encode_video: plan.video == StreamMode::Encode,
        encode_audio: plan.audio == StreamMode::Encode,
        video_caps: kahawai_media::remux::source_caps_names("video", info),
        audio_caps: kahawai_media::remux::source_caps_names("audio", info),
        needs_tonemap: plan.tone_map,
        needs_ass_burn: burns_ass,
        // Audio-only sessions stay local (`Registry::place`), while a session
        // already bound to a full transcoder remains remote even if a track
        // switch changes video to copy. Preserve the feature for failover.
        required_protocol_feature: loudness_protocol_feature(plan),
        video_codec: if plan.video == StreamMode::Encode {
            plan.video_codec.as_str().to_string()
        } else {
            String::new()
        },
        audio_codec: if plan.audio == StreamMode::Encode {
            plan.audio_codec.as_str().to_string()
        } else {
            String::new()
        },
        work_class: (!class.is_empty()).then(|| class.clone()),
        source_kbps: info
            .duration_ms
            .filter(|duration| *duration > 0)
            .map(|duration| {
                ((parts.iter().map(|part| part.size).sum::<u64>() * 8) / duration) as u32
            }),
    };
    (need, class)
}

/// What the box that would run an encode can do. A PARAMETER rather
/// than part of [`Negotiation`], because the two callers learn it from
/// different places and the difference is load-bearing: a session start
/// probes the whole fleet, while a seek reads it off the executor it is
/// already bound to (a seek cannot move boxes, HUB-15b). Folding these
/// in would quietly make a seek re-plan against a fleet it cannot use.
pub(crate) struct ExecutorFacts {
    /// HUB-15a: the selected full video executor reports tone-map.
    pub tonemap: bool,
    /// HUB-15b: verified video targets of that full executor.
    pub video_targets: Vec<String>,
    /// Its audio targets, used when the whole video-encode pipeline is
    /// dispatched there.
    pub full_audio_targets: Vec<String>,
    /// Audio targets of the hub's lightweight local worker.
    pub local_audio_targets: Vec<String>,
    /// Additive protocol features understood by the selected full executor.
    pub full_protocol: kahawai_proto::ProtocolFeatures,
    /// HUB-32b: this source's display-set timeline is readable where
    /// the encode would run.
    pub burn_capable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SourceChoiceKey {
    pub(super) incomplete: bool,
    pub(super) ordinary_cost: kahawai_media::negotiate::Cost,
    pub(super) force_missed: bool,
}

/// Cross-rendition choice is based on what each source costs before an
/// optional force preference changes its audio. Force capability breaks ties;
/// it never promotes a source whose ordinary video path is more expensive.
pub(super) fn source_choice_key(
    ordinary: &kahawai_media::negotiate::SourcePlan,
    force_missed: bool,
) -> SourceChoiceKey {
    SourceChoiceKey {
        incomplete: ordinary.incomplete,
        ordinary_cost: ordinary.cost,
        force_missed,
    }
}

pub(super) struct SourceChoice {
    pub(super) plan: kahawai_media::negotiate::SourcePlan,
    pub(super) index: usize,
    pub(super) measurement: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
    pub(super) key: SourceChoiceKey,
}

/// Everything a negotiation needs that does not change between
/// candidates: the caller's identity-derived facts and their picks.
///
/// Extracted from `start_inner`, where it was five closures over the
/// same captures, called at five points — twice to choose a source and
/// three more times to re-plan after a fetch withdrew a tier. Holding
/// it in one place is what lets a read-only caller ask the same
/// question without starting a session.
pub(crate) struct Negotiation<'a> {
    pub(super) registry: &'a Registry,
    pub(super) sessions: &'a Sessions,
    /// The client's profile, already tightened by the user's standing
    /// bandwidth cap (HUB-15).
    pub(super) profile: kahawai_core::media::CapabilityProfile,
    /// Default-on measured loudness normalization policy.
    pub(super) loudness: LoudnessPreference,
    /// HUB-32a/d: the user's ASS ladder. `pub` because the overlay rung
    /// only becomes real once something has been rasterised, and the
    /// caller flips `overlay_ready` before re-planning.
    pub ass: kahawai_media::negotiate::AssPolicy,
    /// HUB-32c: image tracks that already have OCR text derived from
    /// them, keyed per source.
    pub(super) ocr_set: std::collections::HashSet<(String, String, String, String, i64)>,
    pub(super) raster_sources: Option<std::collections::HashSet<FileId>>,
    /// An explicit burn pick, if the caller named one and it is a track
    /// some tier could actually burn.
    pub(super) burn_row: Option<crate::tracks::Track>,
    pub(super) audio_track: u32,
    /// QUERY resolves each rendition's preference before ranking. Keyed by
    /// first-part file ID after resolving the public source IDs once.
    pub(super) source_audio_tracks: std::collections::HashMap<FileId, u32>,
    pub(super) video_track: u32,
    /// The chosen source has a current measurement and force may therefore
    /// turn only its audio copy/direct path into an encode.
    pub(super) force_audio_encode: bool,
    pub(super) force_measurement: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
}

impl<'a> Negotiation<'a> {
    /// Resolve the caller's inputs once. The subtitle pick is validated
    /// here so a bad track id fails before any source work.
    #[allow(clippy::too_many_arguments)] // the caller's request, spelled out
    pub(crate) async fn preferences(
        sessions: &'a Sessions,
        registry: &'a Registry,
        user_id: &str,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        audio_track: u32,
        video_track: u32,
    ) -> Result<Self> {
        // ONE path: every session negotiates. The user's standing
        // bandwidth cap tightens whatever the client asked for (HUB-15).
        let mut profile = profile.unwrap_or_default();
        let pref_cap: Option<u32> = sqlx::query_scalar::<_, String>(
            "SELECT value FROM user_prefs
              WHERE user_id = ? AND scope = '' AND key = 'bandwidth_kbps'",
        )
        .bind(user_id)
        .fetch_optional(registry.db())
        .await?
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0);
        profile.max_bandwidth_kbps = match (profile.max_bandwidth_kbps, pref_cap) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let loudness = registry.loudness_normalization(user_id).await?;
        // HUB-32a: burn ASS or flatten it? A fleet fact and this user's
        // standing choice. Fleet-wide rather than per-box on purpose —
        // the tier is decided before placement, which then hard-filters
        // on it (registry::PlacementNeed::needs_ass_burn).
        let local_ass_burn =
            registry.local_video_executor_enabled() && kahawai_media::remux::ass_burn_available();
        let ass = crate::tracks::ass_policy_for_user(
            registry.db(),
            user_id,
            registry.any_transcoder_ass_burn() || local_ass_burn,
        )
        .await;
        Ok(Self {
            registry,
            sessions,
            profile,
            loudness,
            ass,
            ocr_set: Default::default(),
            raster_sources: None,
            burn_row: None,
            audio_track,
            source_audio_tracks: Default::default(),
            video_track,
            force_audio_encode: false,
            force_measurement: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn catalogue_subtitle(
        &mut self,
        input: &CataloguePlayback,
        item: &str,
        track: Option<i64>,
    ) -> Result<()> {
        if let Some(id) = track {
            let selected = input
                .media_entry_id
                .as_ref()
                .and_then(|id| input.item.renditions.iter().find(|r| r.entry.id == *id));
            let candidates = input.candidates()?;
            let picked = selected
                .and_then(|r| {
                    candidates.iter().find(|(p, _)| {
                        r.files
                            .first()
                            .is_some_and(|f| p[0].file_id == FileId::Catalogue(f.id.clone()))
                    })
                })
                .map(|(p, info)| -> Result<_> {
                    Ok(catalogue::tracks(item, &p[0], info)
                        .into_iter()
                        .chain(input.capture(p, item)?.subtitles)
                        .find(|t| t.id == id))
                })
                .transpose()?
                .flatten()
                .ok_or_else(|| NoSuchTrack {
                    item: item.into(),
                    track: id,
                })?;
            self.burn_row = Some(picked).filter(|t| {
                crate::tracks::is_image_format(&t.format)
                    || matches!(t.format.as_str(), "ass" | "ssa")
            });
        }
        Ok(())
    }

    pub(crate) fn catalogue_audio_tracks(
        &mut self,
        input: &CataloguePlayback,
        tracks: &std::collections::BTreeMap<String, u32>,
    ) {
        for rendition in &input.item.renditions {
            if let (Some(file), Some(track)) =
                (rendition.files.first(), tracks.get(&rendition.entry.id))
            {
                self.source_audio_tracks
                    .insert(FileId::Catalogue(file.id.clone()), *track);
            }
        }
    }

    pub(crate) fn profile(&self) -> &kahawai_core::media::CapabilityProfile {
        &self.profile
    }

    pub(super) fn audio_track_for(&self, parts: &[PartSource]) -> u32 {
        parts
            .first()
            .and_then(|part| self.source_audio_tracks.get(&part.file_id).copied())
            .unwrap_or(self.audio_track)
    }

    /// The burn pick, but only when it belongs to THESE parts — a pick
    /// naming another source is not a pick for this one.
    pub(crate) fn pick_for(
        &self,
        parts: &[PartSource],
    ) -> Option<kahawai_media::negotiate::BurnPick> {
        let t = self.burn_row.as_ref()?;
        let p = parts.first()?;
        if (
            t.module_id.as_deref(),
            t.collection_id.as_deref(),
            t.root_token.as_deref(),
            t.source_path.as_deref(),
        ) != (
            Some(p.module_id.as_str()),
            Some(p.collection_id.as_str()),
            Some(p.root_token.as_str()),
            Some(p.path_rel.as_str()),
        ) {
            return None;
        }
        let i = t.stream_index? as usize;
        match t.origin.as_str() {
            "embedded" => Some(kahawai_media::negotiate::BurnPick::Embedded(i)),
            "sidecar" | "downloaded" => Some(kahawai_media::negotiate::BurnPick::Sidecar(i)),
            _ => None,
        }
    }

    /// Ask the fleet which box would run an encode of this source, and
    /// what it can do. A pure query — `pick_transcoder` is
    /// `choose(need, false)` and reserves nothing.
    pub(crate) fn probe(
        &self,
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        required_protocol_feature: Option<kahawai_proto::ProtocolFeature>,
    ) -> ExecutorFacts {
        // HUB-15a: would the box that runs a video encode of THIS
        // source tone-map? Same placement question the real dispatch
        // asks (ponytail: probed with encode_audio=false — a fleet
        // where that changes the pick diverges cosmetically; the
        // worker-side guard keeps the failure soft).
        let need = crate::registry::PlacementNeed {
            encode_video: true,
            encode_audio: false,
            video_caps: kahawai_media::remux::source_caps_names("video", info),
            audio_caps: vec![],
            needs_tonemap: true,
            // Gain fields are additive on the wire but cannot degrade to an
            // old worker silently ignoring normalization. The ordinary probe
            // stays broad and is repeated with the exact required feature only
            // after the plan proves it needs gain.
            required_protocol_feature,
            // Not yet known here: the probe runs before the plan picks
            // a subtitle tier, and asking for the burn would narrow the
            // pool that DECIDES it.
            needs_ass_burn: false,
            // Codec-agnostic probe: "which box would run an encode of
            // this source at all" — its verified encoder set then
            // becomes negotiation's target pool (HUB-15b), same shape
            // as the tone-map fact.
            video_codec: String::new(),
            audio_codec: String::new(),
            // HUB-36: the probe asks WHICH BOX, not how fast — there is
            // no plan yet to classify, so it carries no prediction
            // inputs and ranks exactly as it did before phase 5.
            work_class: None,
            source_kbps: None,
        };
        let local_audio_targets = local_audio_encoder_names();
        let (tonemap, video_targets, full_audio_targets, full_protocol) =
            match self.registry.pick_transcoder(&need) {
                Some(tc) => {
                    let targets = self.registry.transcoder_encoders(&tc);
                    (
                        self.registry.transcoder_reports_tonemap(&tc),
                        video_encoder_names(&targets),
                        audio_encoder_names(&targets),
                        self.registry
                            .transcoder_protocol_features(&tc)
                            .unwrap_or_default(),
                    )
                }
                None if self.registry.local_video_executor_enabled() => (
                    local_tonemap_available(self.registry),
                    local_video_encoder_names(self.registry),
                    local_audio_targets.clone(),
                    kahawai_proto::ProtocolFeatures::current(),
                ),
                None => (false, Vec::new(), Vec::new(), Default::default()),
            };
        ExecutorFacts {
            tonemap,
            video_targets,
            full_audio_targets,
            full_protocol,
            local_audio_targets,
            burn_capable,
        }
    }

    pub(super) fn plan_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        facts: &ExecutorFacts,
        force_audio_encode: bool,
    ) -> kahawai_media::negotiate::SourcePlan {
        let mut ass = self.ass.clone();
        if let Some(sources) = &self.raster_sources {
            ass.overlay_ready = parts.first().is_some_and(|p| sources.contains(&p.file_id));
        }
        let est_kbps = info
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| ((parts.iter().map(|p| p.size).sum::<u64>() * 8) / d) as u32);
        let ocr_flags = parts
            .first()
            .map(|p| {
                crate::subtitles::ocr_flags_for(
                    &self.ocr_set,
                    &p.module_id,
                    &p.collection_id,
                    &p.root_token,
                    &p.path_rel,
                    info.subtitles.len(),
                )
            })
            .unwrap_or_default();
        let negotiate = |force| {
            kahawai_media::negotiate::negotiate_for_executors(
                &self.profile,
                info,
                self.audio_track_for(parts) as usize,
                self.video_track as usize,
                parts.len() == 1,
                est_kbps,
                facts.tonemap,
                facts.burn_capable,
                &ocr_flags,
                self.pick_for(parts),
                &ass,
                &facts.video_targets,
                &facts.full_audio_targets,
                &facts.local_audio_targets,
                force,
            )
        };
        let normal = negotiate(false);
        if !force_audio_encode {
            return normal;
        }
        negotiate(true)
    }

    pub(super) fn plans_with_probe(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> (
        kahawai_media::negotiate::SourcePlan,
        kahawai_media::negotiate::SourcePlan,
    ) {
        let ordinary_facts = self.probe(info, burn_capable, None);
        let ordinary = self.plan_with_force(parts, info, &ordinary_facts, false);
        let Some(measurement) = measurement else {
            return (ordinary.clone(), ordinary);
        };
        let forced = self.plan_with_force(parts, info, &ordinary_facts, true);
        let usable_force = |candidate: &kahawai_media::negotiate::SourcePlan| {
            candidate.cost != kahawai_media::negotiate::Cost::Unplayable
                && candidate.plan.audio == kahawai_media::remux::StreamMode::Encode
                && candidate.plan.video == ordinary.plan.video
        };
        let selected = if !usable_force(&forced) {
            ordinary.clone()
        } else {
            let mut measured_plan = forced.plan;
            apply_audio_loudness_measurement(
                &mut measured_plan,
                LoudnessPreference::Force,
                Some(measurement.clone()),
            );
            let required = (measured_plan.video == kahawai_media::remux::StreamMode::Encode)
                .then(|| loudness_protocol_feature(&measured_plan))
                .flatten();
            if required.is_none_or(|feature| ordinary_facts.full_protocol.supports(feature)) {
                forced
            } else {
                let exact_facts = self.probe(info, burn_capable, required);
                let exact = self.plan_with_force(parts, info, &exact_facts, true);
                if usable_force(&exact) {
                    exact
                } else {
                    ordinary.clone()
                }
            }
        };
        (ordinary, selected)
    }

    pub(super) fn plan_for_protocol(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        required_protocol_feature: Option<kahawai_proto::ProtocolFeature>,
    ) -> kahawai_media::negotiate::SourcePlan {
        let facts = self.probe(info, burn_capable, required_protocol_feature);
        self.plan_with_force(parts, info, &facts, false)
    }

    pub(super) fn plan_with_probe(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plans_with_probe(parts, info, burn_capable, measurement)
            .1
    }

    /// Probe the fleet, then plan. The session-start form.
    pub(crate) fn plan_probed(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plan_with_probe(parts, info, burn_capable, self.force_measurement.as_ref())
    }

    /// HUB-32b: the display-set timeline comes from the mediahost,
    /// which walks its own disk in milliseconds — the hub cannot, every
    /// read would cross the byte plane at ~4 KB/s. Offer the tier while
    /// that host is reachable; the sets themselves decide later, and a
    /// failure there re-plans.
    pub(super) fn reads_sets_for(&self, parts: &[PartSource]) -> bool {
        parts.first().is_some_and(|p| {
            self.registry.is_connected(&p.module_id)
                || self.sessions.bytes.reads_locally(&p.module_id)
        })
    }

    pub(super) fn plan_auto_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plans_auto_with_force(parts, info, measurement).1
    }

    pub(super) fn plans_auto_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> (
        kahawai_media::negotiate::SourcePlan,
        kahawai_media::negotiate::SourcePlan,
    ) {
        self.plans_with_probe(parts, info, self.reads_sets_for(parts), measurement)
    }

    pub(super) async fn forced_measurement(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
    ) -> Result<Option<kahawai_media::loudness::AudioLoudnessMeasurement>> {
        if !self.loudness.force() || parts.len() != 1 || info.audio.is_empty() {
            return Ok(None);
        }
        let audio_track = (self.audio_track_for(parts) as usize).min(info.audio.len() - 1);
        parts[0]
            .file_id
            .audio_loudness(
                self.registry,
                audio_track,
                parts[0].size,
                parts[0].mtime_unix,
            )
            .await
    }

    pub(crate) async fn choose_sources(
        &mut self,
        mut eligible: Vec<(Vec<PartSource>, kahawai_core::media::MediaInfo)>,
        mode: Option<&str>,
    ) -> Result<(
        Vec<PartSource>,
        kahawai_core::media::MediaInfo,
        kahawai_media::negotiate::SourcePlan,
        String,
    )> {
        anyhow::ensure!(!eligible.is_empty(), "no sources for item");
        eligible.retain(|(parts, _)| {
            parts
                .iter()
                .all(|p| self.registry.is_connected(&p.module_id))
        });
        if eligible.is_empty() {
            bail!(SourceOffline);
        }
        // Whole renditions stay intact; quality only breaks an equal playback cost.
        eligible.sort_by_key(|(_, info)| {
            std::cmp::Reverse(info.video.first().map(|v| v.height).unwrap_or(0))
        });
        match mode {
            // Operator override (scripts, pipeline debugging): explicit direct
            // still means original bytes. An explicit remux may force gain.
            Some(m) => {
                let (parts, info) = eligible.remove(0);
                let measurement = if m == "direct" {
                    None
                } else {
                    self.forced_measurement(&parts, &info).await?
                };
                let force = measurement.is_some();
                let sp = self.plan_auto_with_force(&parts, &info, measurement.as_ref());
                self.force_audio_encode = force;
                self.force_measurement = measurement;
                Ok((parts, info, sp, m.to_string()))
            }
            // HUB-14/16: judge every candidate, cheapest sufficient
            // path wins, rank breaks ties.
            None => {
                let mut candidates = eligible;
                // A burn pick pins the source it binds to: judging the
                // others would let a cheaper copy win and silently drop
                // the burn the user explicitly selected. Reaching here means
                // sources DO exist and none of them carries the pinned
                // track, which no amount of waiting fixes.
                if self.burn_row.is_some() {
                    candidates.retain(|(parts, _)| self.pick_for(parts).is_some());
                    if candidates.is_empty() {
                        bail!("the picked subtitle track's source is not available");
                    }
                }
                // Completeness and the ORDINARY cost choose the rendition.
                // Force may break that tie, then replace only the winner's
                // audio plan. Ranking the forced plan itself made a measured
                // HEVC encode beat an unmeasured H.264 direct source, which
                // turned an audio-only preference into video transcoding.
                let mut best: Option<SourceChoice> = None;
                for (idx, (parts, info)) in candidates.iter().enumerate() {
                    let measurement = self.forced_measurement(parts, info).await?;
                    let (ordinary, sp) =
                        self.plans_auto_with_force(parts, info, measurement.as_ref());
                    let normalized = measurement.is_some()
                        && sp.plan.audio == kahawai_media::remux::StreamMode::Encode;
                    let force_missed = self.loudness.force() && !normalized;
                    let key = source_choice_key(&ordinary, force_missed);
                    let better = best.as_ref().is_none_or(|current| key < current.key);
                    if better {
                        best = Some(SourceChoice {
                            plan: sp,
                            index: idx,
                            measurement,
                            key,
                        });
                    }
                }
                let SourceChoice {
                    plan: sp,
                    index: idx,
                    measurement,
                    ..
                } = best.unwrap();
                self.force_audio_encode = measurement.is_some();
                self.force_measurement = measurement;
                let mode = if sp.direct { "direct" } else { "remux" };
                let (parts, info) = candidates.into_iter().nth(idx).unwrap();
                Ok((parts, info, sp, mode.to_string()))
            }
        }
    }
}

/// The local process's benchmark-verified VIDEO encoder codec names —
/// negotiation's video target pool when no fleet box would run the encode
/// (HUB-15b), mirroring the `tonemap_available()` fallback. Audio encoders are
/// discovered separately by [`local_audio_encoder_names`].
pub(super) fn local_video_encoder_names(registry: &Registry) -> Vec<String> {
    let Some(bench) = registry.local_bench() else {
        return Vec::new();
    };
    kahawai_media::remux::encoder_capabilities()
        .iter()
        .filter(|(_, element, _)| bench.encoder_ready(element))
        .map(|(codec, _, _)| codec.to_string())
        .collect()
}

pub(super) fn local_tonemap_available(registry: &Registry) -> bool {
    registry
        .local_bench()
        .is_some_and(|bench| bench.tonemap_ready())
        && kahawai_media::remux::tonemap_available()
}

pub(super) fn local_audio_encoder_names() -> Vec<String> {
    [
        ("aac", kahawai_media::remux::aac_encoder()),
        ("opus", kahawai_media::remux::opus_encoder()),
    ]
    .into_iter()
    .filter_map(|(codec, element)| element.map(|_| codec.to_string()))
    .collect()
}

pub(super) fn video_encoder_names(targets: &[String]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| matches!(t.as_str(), "h264" | "hevc" | "av1"))
        .cloned()
        .collect()
}

pub(super) fn audio_encoder_names(targets: &[String]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| matches!(t.as_str(), "aac" | "opus"))
        .cloned()
        .collect()
}

/// The negotiation speaks stream indexes; the API speaks unified track
/// rows. Stamp each verdict with the id of the embedded row bound to
/// the session's source (missing rows read as None — a source scanned
/// before the unification migration ran).
pub(crate) async fn fill_verdict_track_ids(
    _registry: &Registry,
    parts: &[PartSource],
    verdicts: &mut [kahawai_media::negotiate::SubtitleVerdict],
) {
    if parts.is_empty() {
        return;
    }
    for verdict in verdicts {
        verdict.track_id = Some(verdict.index as i64 + 1);
    }
}

/// Fold a worker's session facts (AR-13) into the per-kind verdict, so
/// "dts → aac (transcoded)" becomes "dts → aac (transcoded) · 7.1 → 5.1".
/// Idempotent — a seek-restart re-learns the same facts and must not
/// stutter them — and unknown kinds are logged rather than lost.
pub(super) fn fold_facts(
    verdict: &mut Option<(String, String)>,
    facts: &[kahawai_media::facts::Fact],
) {
    let Some((video, audio)) = verdict.as_mut() else {
        return;
    };
    for f in facts {
        let slot = match f.kind.as_str() {
            "audio" => &mut *audio,
            "video" => &mut *video,
            other => {
                tracing::warn!(kind = other, detail = %f.detail, "unroutable session fact");
                continue;
            }
        };
        if !slot.contains(&f.detail) {
            slot.push_str(&format!(" · {}", f.detail));
        }
    }
}
