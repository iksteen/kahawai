use super::*;

/// How long a burn-in session waits for the mediahost to walk its
/// index. Milliseconds on local disk; this is the sanity bound, well
/// inside the client's own patience.
pub(super) const BURN_SETS_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

impl Sessions {
    /// Start a session for an item. With an explicit `mode` (scripts,
    /// debugging, old clients) the pre-negotiation behavior applies
    /// verbatim: best-ranked source, mode taken at its word. Without
    /// one, the hub NEGOTIATES (HUB-14): the client's capability
    /// profile — or the conservative fallback — is judged against
    /// every candidate source and the cheapest sufficient path wins
    /// (HUB-16: direct > copy > audio-encode > video-encode), rank
    /// breaking ties.
    #[allow(clippy::too_many_arguments)] // request-shaped plumbing
    /// Mint the session id FIRST, then do the work.
    ///
    /// Everything below can fail — a source that is not currently
    /// available, an unplayable plan, a worker that will not start — and
    /// until an id exists a failure has nothing to attach a log to. That
    /// is why the id is minted here rather than deeper in: "no source is
    /// currently available (mediahost offline)" used to leave a 409 and
    /// no record whatsoever (OPS-10).
    #[allow(clippy::too_many_arguments)] // one call site, spelled out
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        subtitles: &crate::subtitles::Subtitles,
        user_id: &str,
        item_id: &str,
        mode: Option<&str>,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        start: StartOptions,
        audio_track: u32,
        video_track: u32,
        subtitle_track: Option<i64>,
    ) -> Result<Arc<Session>> {
        let id = ulid::Ulid::generate().to_string();
        self.note_session(&id, item_id);
        // The one admission point. Here rather than inside `start_inner`
        // because that function has thirteen early returns, every one of
        // which would otherwise have to remember to give the slot back.
        self.admit(&id, user_id)?;
        let started = {
            // ...and a fourteenth exit a trailing statement cannot cover: the
            // caller going away. `start_session` awaits this inline, so an
            // abandoned request — a closed tab, or a client that gave up on a
            // slow start — drops this future mid-`start_inner` and any code
            // after the await simply never runs. The id stayed in `reserved`
            // for the life of the process and went on counting, so four
            // abandoned starts left an account unable to begin anything until
            // the hub was restarted. Dropping is the one exit every path has.
            let _slot = SlotGuard {
                sessions: self,
                id: id.clone(),
            };
            self.start_inner(
                &id,
                registry,
                subtitles,
                user_id,
                item_id,
                mode,
                profile,
                start,
                audio_track,
                video_track,
                subtitle_track,
            )
            .await
        };
        if let Err(e) = &started
            && let Some(data_dir) = self.scratch_root.parent()
        {
            // A session that never came up still gets a log, filed under
            // the item like any other — the item page is where somebody
            // looks after a report, and it cannot know how far the
            // session got.
            self.note_error(&id, &format!("{e:#}"));
            let (item, header) = self.log_header(&id);
            crate::sessionlog::store(data_dir, &item, &id, &header);
        }
        if started.is_ok()
            && let Some(data_dir) = self.data_dir()
        {
            let (item, header) = self.log_header(&id);
            crate::sessionlog::store(data_dir, &item, &id, &header);
        }
        started
    }

    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub(super) async fn start_inner(
        self: &Arc<Self>,
        id: &str,
        registry: &Registry,
        subtitles: &crate::subtitles::Subtitles,
        user_id: &str,
        item_id: &str,
        mode: Option<&str>,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        start: StartOptions,
        audio_track: u32,
        video_track: u32,
        subtitle_track: Option<i64>,
    ) -> Result<Arc<Session>> {
        let id = id.to_string();
        let mut neg =
            Negotiation::preferences(self, registry, user_id, profile, audio_track, video_track)
                .await?;
        let input = start
            .catalogue
            .as_ref()
            .context("catalogue playback required")?;
        neg.catalogue_subtitle(input, item_id, subtitle_track)?;
        let (parts, info, sp, mode) = input
            .negotiate(&mut neg, mode, start.source_fingerprint.as_deref())
            .await?;
        let mut catalogue = input.capture(&parts, item_id)?;
        let captured = catalogue.clone();
        let resume_fingerprint = catalogue::fingerprint(&parts);
        let start_ms = {
            let saved = crate::watch::read(registry.db(), user_id, &[item_id.to_owned()])
                .await?
                .remove(item_id)
                .unwrap_or_default();
            if start.resume && !start.explicit_position {
                saved.resume_position_ms.unwrap_or(0) as u64
            } else {
                start.ms
            }
        };
        for part in &parts {
            registry.hint_discovery(
                &part.module_id,
                "segments",
                &part.collection_id,
                kahawai_proto::v1::SourcePath::new(&part.root_token, &part.path_rel),
                "playback",
                15 * 60,
            );
        }
        // HUB-32b: a burn is only real once the display sets exist. Ask
        // the mediahost, and if they do not arrive, negotiate again
        // with the tier withdrawn — better an honest "unavailable"
        // than an encode that burns nothing.
        let mut burn_sets: Option<std::path::PathBuf> = None;
        let mut sp = sp;
        // Whatever the chosen source was judged with — a later re-plan
        // must not resurrect a tier an earlier one withdrew.
        let mut burn_capable = parts.first().is_some_and(|p| {
            registry.is_connected(&p.module_id) || self.bytes.reads_locally(&p.module_id)
        });
        // What to walk: the media file at the embedded index, or — for
        // a sidecar pick — the .idx at its in-idx track.
        let sets_ref = match (sp.plan.burn_subtitle, sp.burn_sidecar) {
            (Some(idx), _) => parts.first().map(|p| (p.path_rel.clone(), idx)),
            (None, Some(i)) => info
                .external_subtitles
                .get(i)
                .filter(|e| e.format == "vobsub")
                .map(|e| (e.path_rel.clone(), e.track.unwrap_or(0) as usize)),
            (None, None) => None,
        };
        if let Some((walk_rel, walk_idx)) = sets_ref
            && let Some(part) = parts.first()
        {
            burn_sets = subtitles
                .image_sets(
                    registry,
                    &part.module_id,
                    &part.collection_id,
                    &part.root_token,
                    &walk_rel,
                    walk_idx,
                    &if sp.burn_sidecar.is_some() {
                        catalogue::physical(part, &info).sidecar_revision
                    } else {
                        catalogue::physical(part, &info).revision
                    },
                    BURN_SETS_WAIT,
                    false,
                )
                .await;
            if burn_sets.is_none() {
                tracing::warn!(
                    item = item_id,
                    track = walk_idx,
                    "burn-in: no display sets; re-planning without it"
                );
                // burn_capable=false also voids the pick: negotiate
                // ignores a pick it cannot honor.
                burn_capable = false;
                sp = neg.plan_probed(&parts, &info, false);
            }
        }
        // HUB-32d: the overlay rung only exists once a rasterised track
        // does. Asked AFTER the source is chosen and only when the
        // ladder would actually take it — rasterising for a client
        // that would flatten anyway is pure waste — and the answer
        // re-plans, exactly as failing display sets do one tier down.

        if neg.ass.overlay_reachable(neg.profile()) {
            let captured = &mut catalogue;
            let parents = catalogue::tracks(item_id, &parts[0], &info)
                .into_iter()
                .chain(captured.subtitles.iter().cloned())
                .collect::<Vec<_>>();
            let selected = neg.burn_row.as_ref().map(|t| t.id);
            if let Some(parent) = parents.iter().find(|t| {
                matches!(t.format.as_str(), "ass" | "ssa") && selected.is_none_or(|id| t.id == id)
            }) {
                match tokio::time::timeout(
                    crate::subtitles::RASTER_WAIT,
                    subtitles.catalogue_raster(registry, &self.bytes, parent),
                )
                .await
                {
                    Ok(Ok(())) => {
                        *captured = input.capture(&parts, item_id)?;
                        neg.ass.overlay_ready = true;
                        neg.raster_sources
                            .get_or_insert_default()
                            .insert(parts[0].file_id.clone());
                        sp = neg.plan_probed(&parts, &info, burn_capable);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "catalogue ASS raster unavailable; using next tier")
                    }
                    Err(_) => {
                        tracing::info!("catalogue ASS raster still rendering; using next tier")
                    }
                }
            }
        }
        // HUB-32a: a sidecar ASS burn needs the script itself, the same
        // way an image burn needs its display sets — the worker cannot
        // read the media's neighbourhood. Embedded burns need nothing:
        // they take the demuxer's own pad, which also carries the fonts.
        let mut burn_ass_text: Option<String> = None;
        if let Some(i) = sp.burn_ass_sidecar {
            let part = &parts[0];
            let tracks: Vec<crate::tracks::Track> = {
                let captured = &catalogue;
                catalogue::tracks(item_id, part, &info)
                    .into_iter()
                    .chain(captured.subtitles.iter().cloned())
                    .collect()
            };
            if let Some(track) = tracks.iter().find(|t| {
                matches!(t.origin.as_str(), "sidecar" | "downloaded")
                    && t.stream_index == Some(i as i64)
            }) {
                burn_ass_text = subtitles.ass_for_burn(registry, &self.bytes, track).await;
            }
            if burn_ass_text.is_none() {
                // Same honesty rule as the display sets: re-plan with
                // the tier withdrawn rather than encode video that burns
                // nothing.
                tracing::warn!(
                    item = item_id,
                    track = i,
                    "ASS burn: sidecar script unavailable; re-planning without it"
                );
                sp = neg.plan_probed(&parts, &info, burn_capable);
            }
        }
        // No refusal to raise: the ladder is a permutation and flatten
        // is always possible, so `AssPolicy::choose` is total and a burn
        // is only ever planned when some box can perform it.
        let mut burns_ass = sp.plan.burn_ass.is_some() || sp.burn_ass_sidecar.is_some();
        if sp.cost == kahawai_media::negotiate::Cost::Unplayable && mode != "direct" {
            // The verdict names the actual blocker — a client refusing
            // the encode target reads very differently from a fleet
            // with no transcoder (HUB-14 honesty; found via the mask).
            bail!(
                "no playable streams: {} · {}",
                sp.video_verdict,
                sp.audio_verdict
            );
        }
        let mut negotiated = sp;
        let mode = mode.as_str();
        if parts.len() > 1 && mode == "direct" {
            bail!("multi-part sources play via remux/transcode, not direct");
        }
        let total_ms: u64 = parts.iter().map(|p| p.duration_ms).sum();
        let start_idx = part_index(&parts, start_ms);
        let part = parts[start_idx].clone();
        let local_ms = start_ms.saturating_sub(part.base_ms);
        let (module_id, path_rel, size) =
            (part.module_id.clone(), part.path_rel.clone(), part.size);
        let lease = self
            .bytes
            .open_lease(
                registry,
                &part.module_id,
                &part.collection_id,
                &part.root_token,
                &part.path_rel,
                Reader::Viewer,
            )
            .await?;

        let mut chosen_sink = String::new();
        let mut verdict = None;
        let mut session_plan = None;
        let mut session_needs = crate::registry::PlacementNeed::default();
        // HUB-36: the kind of work this session IS, derived here because
        // this is where the plan and the source metadata are both in
        // scope. Whatever box runs it reports a pace sample against this
        // string, so the two can never describe different things.
        let mut session_class = String::new();
        let session_mode = match mode {
            "direct" => Mode::Direct { lease },
            "remux" => {
                // The muxer stalls on unfed pads, so only claim what the
                // plan will actually feed — the negotiated plan is the
                // single source of truth with the pipeline's link logic.
                let ordinary_negotiated = if neg.loudness.force() {
                    neg.plan_for_protocol(&parts, &info, burn_capable, None)
                } else {
                    negotiated.clone()
                };
                let mut plan = negotiated.plan;
                if !plan.playable() {
                    bail!(
                        "no playable streams: {} · {}",
                        negotiated.video_verdict,
                        negotiated.audio_verdict
                    );
                }
                fill_audio_loudness_gains(
                    registry,
                    &parts,
                    &mut plan,
                    neg.loudness,
                    neg.force_measurement.clone(),
                )
                .await?;
                if !neg.loudness.force()
                    && plan.video == kahawai_media::remux::StreamMode::Encode
                    && let Some(required) = loudness_protocol_feature(&plan)
                {
                    let mut candidate =
                        neg.plan_for_protocol(&parts, &info, burn_capable, Some(required));
                    if !candidate.plan.playable()
                        || candidate.incomplete != ordinary_negotiated.incomplete
                        || candidate.plan.audio != plan.audio
                        || !same_video_path(&candidate.plan, &plan)
                        || candidate.burn_sidecar != ordinary_negotiated.burn_sidecar
                        || candidate.burn_ass_sidecar != ordinary_negotiated.burn_ass_sidecar
                    {
                        // Default normalization is optional and must not alter
                        // the video or subtitle path. If no exact-gain worker
                        // can execute that path, preserve playback without gain.
                        apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                    } else {
                        fill_audio_loudness_gains(
                            registry,
                            &parts,
                            &mut candidate.plan,
                            neg.loudness,
                            None,
                        )
                        .await?;
                        plan = candidate.plan;
                        burns_ass = candidate.plan.burn_ass.is_some()
                            || candidate.burn_ass_sidecar.is_some();
                        negotiated = candidate;
                    }
                }
                verdict = Some((
                    negotiated.video_verdict.clone(),
                    negotiated.audio_verdict.clone(),
                ));
                session_plan = Some(plan);
                (session_needs, session_class) = placement_need(&plan, &info, &parts, burns_ass);
                // Encode work goes to the fleet when one is available
                // (§4.5); pure remux — and encode with no fleet — stays
                // in the local supervised worker.
                // HUB-36 phase 5: the placement now carries what it is
                // expected to sustain, so a session that will crawl says
                // so instead of letting the viewer discover it.
                let place = |need: &crate::registry::PlacementNeed| {
                    if need.encode_video || need.encode_audio {
                        registry.place(need)
                    } else {
                        crate::registry::Placement {
                            target: None,
                            available: true,
                            predicted: None,
                        }
                    }
                };
                let mut placement = place(&session_needs);
                if !placement.available && session_needs.required_protocol_feature.is_some() {
                    // Capacity and hard constraints can change after the
                    // compatible probe. Retry the exact ordinary plan with no
                    // protocol requirement rather than turning that race into
                    // a playback failure or a force-only unity-gain encode.
                    negotiated = ordinary_negotiated;
                    plan = negotiated.plan;
                    apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                    burns_ass = plan.burn_ass.is_some() || negotiated.burn_ass_sidecar.is_some();
                    verdict = Some((
                        negotiated.video_verdict.clone(),
                        negotiated.audio_verdict.clone(),
                    ));
                    session_plan = Some(plan);
                    (session_needs, session_class) =
                        placement_need(&plan, &info, &parts, burns_ass);
                    placement = place(&session_needs);
                }
                anyhow::ensure!(
                    plan.playable(),
                    "no playable streams after loudness protocol fallback: {} · {}",
                    negotiated.video_verdict,
                    negotiated.audio_verdict
                );
                anyhow::ensure!(
                    placement.available,
                    "video transcoding unavailable: no capable external transcoder or enabled all-in-one transcoder"
                );
                let placed = placement.target.clone();
                if let Some(p) = placement.predicted
                    && p < 1.0
                {
                    // AR-13: below realtime is placed anyway — refusing
                    // would leave a slow fleet unusable — but it is
                    // never placed SILENTLY.
                    tracing::warn!(
                        session = %id,
                        box_id = placed.as_deref().unwrap_or("local"),
                        class = session_needs.work_class.as_deref().unwrap_or("-"),
                        predicted = p,
                        "placed below realtime; playback may stall"
                    );
                    fold_facts(
                        &mut verdict,
                        &[kahawai_media::facts::Fact {
                            kind: "video".into(),
                            detail: format!("predicted {p:.1}x realtime — may stall"),
                        }],
                    );
                }
                // Read once: the same bytes serve both dispatch attempts.
                let sets_bytes = match &burn_sets {
                    Some(p) => std::fs::read(p).unwrap_or_default(),
                    None => Vec::new(),
                };
                let ass_bytes = burn_ass_text.clone().unwrap_or_default().into_bytes();
                match placed {
                    Some(tc) => {
                        // `place` reserved this box. Exactly one owner
                        // of that reservation: this branch. It is held
                        // across the sink-fallback retry — which reuses
                        // the same box, so releasing between attempts
                        // would leave a successful retry uncounted —
                        // and returned on every failing path below.
                        let dispatched = self
                            .dispatch_to(
                                registry,
                                &tc,
                                &id,
                                plan,
                                &parts,
                                start_idx,
                                local_ms,
                                &sets_bytes,
                                &ass_bytes,
                            )
                            .await;
                        let (facts, sink) = match dispatched {
                            Ok(v) => v,
                            Err(e) => {
                                registry.tc_session_ended(&tc);
                                return Err(e);
                            }
                        };
                        chosen_sink = sink;
                        fold_facts(&mut verdict, &facts);
                        Mode::Transcode {
                            transcoder: Mutex::new(tc),
                        }
                    }
                    None => {
                        let tail = self.open_part_leases(registry, &parts, start_idx).await?;
                        let (runner, facts) = match self
                            .start_remux(
                                &id,
                                plan,
                                negotiated.target_duration_secs,
                                tail,
                                local_ms,
                                "",
                                burn_sets.as_deref(),
                                burn_ass_text.as_deref(),
                            )
                            .await
                        {
                            Ok(r) => r,
                            Err(first)
                                if plan.segment_format
                                    != kahawai_media::remux::SegmentFormat::Ts =>
                            {
                                return Err(first); // fmp4 has no sink fallback
                            }
                            Err(first) => {
                                tracing::warn!(session = %id, error = format!("{first:#}"),
                                    "start failed; retrying with fallback sink");
                                let tail =
                                    self.open_part_leases(registry, &parts, start_idx).await?;
                                let r = self
                                    .start_remux(
                                        &id,
                                        plan,
                                        negotiated.target_duration_secs,
                                        tail,
                                        local_ms,
                                        "hlssink2",
                                        burn_sets.as_deref(),
                                        burn_ass_text.as_deref(),
                                    )
                                    .await
                                    .with_context(|| format!("first attempt: {first:#}"))?;
                                chosen_sink = "hlssink2".into();
                                r
                            }
                        };
                        fold_facts(&mut verdict, &facts);
                        Mode::Remux {
                            runner: Mutex::new(runner),
                            dir: self.scratch_root.join(&id),
                        }
                    }
                }
            }
            other => bail!("unknown mode {other:?} (direct|remux)"),
        };

        // Fill the unified track ids into the verdicts: the negotiation
        // speaks stream indexes, the API speaks track rows.
        let mut sub_verdicts = negotiated.subtitles;
        fill_verdict_track_ids(registry, &parts, &mut sub_verdicts).await;
        let burn_pick = burn_sets.as_ref().and_then(|_| neg.pick_for(&parts));
        let session_duration = if parts.len() > 1 {
            Some(total_ms)
        } else {
            info.duration_ms
        };
        let session = Arc::new(Session {
            catalogue,
            info: info.clone(),
            id,
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            collection_item_id: captured.copy_id,
            playable_source_id: captured.source_id,
            library_item_ids: captured.item_ids,
            source_fingerprint: resume_fingerprint,
            replay_gain: info.replay_gain.clone(),

            effective_start_ms: start_ms,
            last_position_ms: std::sync::atomic::AtomicU64::new(start_ms),
            module_id,
            size,
            container: info.container.clone(),
            duration_ms: session_duration,
            parts,
            current_part: std::sync::atomic::AtomicUsize::new(start_idx),
            mode: session_mode,
            verdict: Mutex::new(verdict),
            sub_verdicts: Mutex::new(sub_verdicts),
            profile: neg.profile().clone(),
            target_duration_secs: negotiated.target_duration_secs,
            burn_sets: Mutex::new(burn_sets.clone()),
            burn_pick: Mutex::new(burn_pick),
            ass: neg.ass.clone(),
            burn_ass_text: Mutex::new(burn_ass_text),
            loudness: neg.loudness,
            force_loudness: neg.force_audio_encode,
            sink: Mutex::new(chosen_sink),
            seek_lock: tokio::sync::Mutex::new(()),
            pending_seek: Mutex::new(None),
            seek_gen: std::sync::atomic::AtomicU64::new(0),
            seek_done: tokio::sync::watch::channel((0, Ok(0))).0,
            plan: Mutex::new(session_plan),
            needs: Mutex::new(session_needs),
            pace_class: session_class,
            touched: Mutex::new(std::time::Instant::now()),
            ending: tokio::sync::RwLock::new(false),
        });
        self.active
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        self.idle.send_replace(false);
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        registry.emit(crate::registry::RegistryEvent::Sessions { kind: "sessions" });
        Ok(session)
    }

    /// Leases for every part from `from` onward, in timeline order.
    ///
    /// A remux pipeline spans the rest of the source, so it needs them
    /// all up front: concat holds each later part blocked until the one
    /// before it ends, but the branch has to exist before that happens.
    /// Costs one lease per remaining part instead of one per session —
    /// paid once, at the start, rather than as a stall at every boundary.
    pub(super) async fn open_part_leases(
        &self,
        registry: &Registry,
        parts: &[PartSource],
        from: usize,
    ) -> Result<Vec<(Lease, u64)>> {
        let mut out = Vec::with_capacity(parts.len().saturating_sub(from));
        for part in &parts[from..] {
            let lease = self
                .bytes
                .open_lease(
                    registry,
                    &part.module_id,
                    &part.collection_id,
                    &part.root_token,
                    &part.path_rel,
                    Reader::Viewer,
                )
                .await?;
            out.push((lease, part.size));
        }
        Ok(out)
    }
}
