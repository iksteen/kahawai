use super::*;

impl Sessions {
    /// Seek-restart (§6): tear the session's pipeline down and start it
    /// again at `position_ms` (keyframe-snapped by the demuxer). Same
    /// session id, same URLs — the client re-attaches to a playlist that
    /// now begins at the offset.
    /// Returns the timeline base of the part the restart landed in
    /// (players add it to the pipeline's local start.pos).
    #[allow(clippy::too_many_arguments)] // request-shaped plumbing
    pub async fn seek(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        subtitles: &crate::subtitles::Subtitles,
        id: &str,
        position_ms: u64,
        audio_track: Option<u32>,
        video_track: Option<u32>,
        subtitle_track: Option<i64>,
    ) -> Result<u64> {
        let session = self.get(id).context("no such session")?;
        // Subtitle unification: an explicit pick may change what burns.
        // Resolved HERE, in request context (bounded like a session
        // start), so the detached executor only restarts with state
        // already in hand.
        let mut replan_subs = false;
        if let Some(tid) = subtitle_track
            && tid == 0
        {
            // Sentinel: withdraw an explicit burn ("subtitles off" /
            // a client-rendered track picked after a burn).
            if session.burn_pick.lock().unwrap().take().is_some() {
                *session.burn_sets.lock().unwrap() = None;
                *session.burn_ass_text.lock().unwrap() = None;
                replan_subs = true;
            }
        } else if let Some(tid) = subtitle_track {
            // Typed, as in `Negotiation::new`. This is the path a LIVE viewer
            // takes when they change subtitles, and a stale id — a track
            // deleted from another tab, or one from before a rescan — arrived
            // through `session_refusal` as 409 "this item cannot be played".
            // The player's `switchBurn` then gave up on a film that was
            // playing perfectly well a second earlier.
            let track = { session.catalogue_track(tid) }.ok_or_else(|| NoSuchTrack {
                item: session.item_id.clone(),
                track: tid,
            })?;
            let part = session.parts.first().context("session has no parts")?;
            let is_image = crate::tracks::is_image_format(&track.format);
            // HUB-32a: an ASS pick has to reach negotiation too, but
            // for a different reason than an image one. It never forces
            // a burn — the `ass_fallback` preference is the only way
            // into that tier — it says WHICH track the tier applies to,
            // so switching language mid-film re-burns the new one
            // instead of silently keeping the first.
            let is_ass = matches!(track.format.as_str(), "ass" | "ssa");
            let new_pick = ((is_image || is_ass)
                && track.module_id.as_deref() == Some(part.module_id.as_str())
                && track.collection_id.as_deref() == Some(part.collection_id.as_str())
                && track.root_token.as_deref() == Some(part.root_token.as_str())
                && track.source_path.as_deref() == Some(part.path_rel.as_str()))
            .then(|| {
                let i = track.stream_index.unwrap_or(0) as usize;
                match track.origin.as_str() {
                    "embedded" => Some(kahawai_media::negotiate::BurnPick::Embedded(i)),
                    "sidecar" | "downloaded" => {
                        Some(kahawai_media::negotiate::BurnPick::Sidecar(i))
                    }
                    _ => None,
                }
            })
            .flatten();
            if (is_image || is_ass) && new_pick.is_none() {
                bail!(
                    "track {tid} is not part of the playing source; restart the session to burn it"
                );
            }
            // No capability check here either: a seek cannot move boxes
            // (HUB-15b), so the re-negotiation below simply walks THIS
            // executor's ladder and lands on the next rung it can serve.
            // Auto-burn already burning this very stream: adopt the
            // pick without touching the sets (no refetch needed).
            let already = matches!(new_pick,
                Some(kahawai_media::negotiate::BurnPick::Embedded(i))
                    if session.plan.lock().unwrap().is_some_and(
                        |pl| pl.burn_subtitle == Some(i) || pl.burn_ass == Some(i)));
            if already {
                *session.burn_pick.lock().unwrap() = new_pick;
            } else if new_pick != *session.burn_pick.lock().unwrap() {
                // A sidecar ASS burns from the FILE, so the script has
                // to be in hand before the restart — the same shape as
                // the display sets below, and for the same reason.
                *session.burn_ass_text.lock().unwrap() = match new_pick {
                    Some(kahawai_media::negotiate::BurnPick::Sidecar(_)) if is_ass => {
                        let text = subtitles.ass_for_burn(registry, &self.bytes, &track).await;
                        anyhow::ensure!(text.is_some(), "subtitle track {tid} has no ASS script");
                        text
                    }
                    _ => None,
                };
                let sets = match new_pick {
                    // An ASS pick has no display sets to walk; the
                    // renderer takes the demuxer's pad or the script.
                    Some(_) if is_ass => None,
                    Some(_) => {
                        let (module_id, collection_id, root_token, walk_rel, walk_idx, _) =
                            subtitles.extract_ref(registry, &track).await?;
                        let sets = subtitles
                            .image_sets(
                                registry,
                                &module_id,
                                &collection_id,
                                &root_token,
                                &walk_rel,
                                walk_idx,
                                track.source_revision()?,
                                BURN_SETS_WAIT,
                                false,
                            )
                            .await;
                        anyhow::ensure!(
                            sets.is_some(),
                            "no display sets for track {tid} (mediahost offline or unindexed)"
                        );
                        sets
                    }
                    // Un-burn: the pick is withdrawn. The re-plan's auto
                    // rules may still burn (no-overlay client, no OCR) —
                    // then the pipeline walks the source itself.
                    None => None,
                };
                *session.burn_sets.lock().unwrap() = sets;
                *session.burn_pick.lock().unwrap() = new_pick;
                replan_subs = true;
            }
        }
        // Register the intent; a burst coalesces to the newest one.
        let my_gen = {
            let generation = session
                .seek_gen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut pending = session.pending_seek.lock().unwrap();
            let next = PendingSeek {
                generation,
                position_ms,
                audio_track,
                video_track,
                replan_subs,
            };
            *pending = Some(PendingSeek::merge(pending.take(), next));
            generation
        };
        let mut done = session.seek_done.subscribe();
        // Detached executor: an HTTP disconnect cancels the REQUEST
        // future, never the restart itself.
        {
            let (this, registry, session) = (self.clone(), registry.clone(), session.clone());
            tokio::spawn(async move {
                let _serialized = session.seek_lock.lock().await;
                // Take whatever intent is newest; None = a prior
                // executor already covered it (we were a burst).
                let Some(todo) = session.pending_seek.lock().unwrap().take() else {
                    return;
                };
                // Inner spawn: a PANIC in the restart still publishes —
                // an unpublished generation would hang every waiter.
                let (this2, registry2, session2) =
                    (this.clone(), registry.clone(), session.clone());
                let outcome = match tokio::spawn(async move {
                    this2.execute_seek(&registry2, &session2, todo).await
                })
                .await
                {
                    Ok(r) => r.map_err(|e| format!("{e:#}")),
                    Err(join) => Err(format!("seek restart panicked: {join}")),
                };
                let _ = session.seek_done.send((todo.generation, outcome));
            });
        }
        // Await the restart that covers this request: ours, or the
        // newer one that superseded it — either way the returned state
        // is what actually plays now. Bounded: a wedged restart turns
        // into an error, never a hung client request.
        let waited = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                {
                    let latest = done.borrow();
                    if latest.0 >= my_gen {
                        return latest.1.clone().map_err(|e| anyhow::anyhow!(e));
                    }
                }
                done.changed().await.context("session torn down mid-seek")?;
            }
        })
        .await;
        match waited {
            Ok(r) => r,
            Err(_) => bail!("seek restart did not settle within 60s"),
        }
    }

    pub(super) async fn execute_seek(
        self: &Arc<Self>,
        registry: &Registry,
        session: &Arc<Session>,
        todo: PendingSeek,
    ) -> Result<u64> {
        let PendingSeek {
            position_ms,
            audio_track,
            video_track,
            replan_subs,
            ..
        } = todo;
        let mut plan =
            (*session.plan.lock().unwrap()).context("session has no restartable pipeline")?;
        session.touch();
        let want_audio = audio_track.map(|t| t as usize).unwrap_or(plan.audio_track);
        let want_video = video_track.map(|t| t as usize).unwrap_or(plan.video_track);
        if want_audio != plan.audio_track || want_video != plan.video_track || replan_subs {
            // Switching tracks re-plans: the new track's codec decides
            // copy vs encode, not the old one's — and a burn-pick
            // change re-plans even with the same tracks.
            let info = session.info.clone();
            // HUB-15a: the executor is already chosen here — ask IT.
            // Plain hub-local audio work is not a video executor.
            let tonemap = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry.transcoder_reports_tonemap(&tc)
                }
                _ if registry.local_video_executor_enabled() => {
                    kahawai_media::remux::tonemap_available()
                }
                _ => false,
            };
            // Mirrors the start path (connected counts: the mediahost
            // walks its own index); sets already fetched into scratch
            // make the burn unconditionally real — a re-plan must not
            // drop a burn whose data is in hand.
            let burn_capable = session.burn_sets.lock().unwrap().is_some()
                || session.parts.first().is_some_and(|p| {
                    registry.is_connected(&p.module_id) || self.bytes.reads_locally(&p.module_id)
                });
            // HUB-15b: re-plans may only pick targets the ALREADY
            // CHOSEN executor encodes — the session does not move boxes
            // on a track switch. A remote video session therefore uses
            // that same box even if the new plan becomes audio-only.
            let (video_targets, full_audio_targets, local_audio_targets) = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    let all = registry.transcoder_encoders(&tc);
                    let video = video_encoder_names(&all);
                    let audio = audio_encoder_names(&all);
                    (video, audio.clone(), audio)
                }
                _ => {
                    let video = if registry.local_video_executor_enabled() {
                        local_video_encoder_names(registry)
                    } else {
                        Vec::new()
                    };
                    let audio = local_audio_encoder_names();
                    (video, audio.clone(), audio)
                }
            };
            let ocr_set = {
                let captured = &session.catalogue;
                catalogue::ocr_sources(&captured.subtitles)
            };
            let ocr_flags = session
                .parts
                .first()
                .map(|p| {
                    crate::subtitles::ocr_flags_for(
                        &ocr_set,
                        &p.module_id,
                        &p.collection_id,
                        &p.root_token,
                        &p.path_rel,
                        info.subtitles.len(),
                    )
                })
                .unwrap_or_default();
            let executor_protocol = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry
                        .transcoder_protocol_features(&tc)
                        .unwrap_or_default()
                }
                _ => kahawai_proto::ProtocolFeatures::current(),
            };
            let force_measurement = if session.force_loudness && session.parts.len() == 1 {
                session.parts[0]
                    .file_id
                    .audio_loudness(
                        registry,
                        want_audio,
                        session.parts[0].size,
                        session.parts[0].mtime_unix,
                    )
                    .await?
            } else {
                None
            };
            let negotiate = |force_audio_encode| {
                kahawai_media::negotiate::negotiate_for_executors(
                    &session.profile,
                    &info,
                    want_audio,
                    want_video,
                    session.parts.len() == 1,
                    None,
                    tonemap,
                    burn_capable,
                    &ocr_flags,
                    // The session's explicit burn keeps forcing across
                    // track switches — its sets are already in hand.
                    *session.burn_pick.lock().unwrap(),
                    // A seek cannot move boxes (HUB-15b), so the question is
                    // whether THIS executor burns ASS — not whether the
                    // fleet does.
                    &kahawai_media::negotiate::AssPolicy {
                        burn_capable: match &session.mode {
                            Mode::Transcode { transcoder } => {
                                let tc = transcoder.lock().unwrap().clone();
                                registry.transcoder_reports_ass_burn(&tc)
                            }
                            _ if registry.local_video_executor_enabled() => {
                                kahawai_media::remux::ass_burn_available()
                            }
                            _ => false,
                        },
                        ..session.ass.clone()
                    },
                    &video_targets,
                    &full_audio_targets,
                    &local_audio_targets,
                    force_audio_encode,
                )
            };
            let mut sp = negotiate(force_measurement.is_some());
            plan = sp.plan;
            fill_audio_loudness_gains(
                registry,
                &session.parts,
                &mut plan,
                session.loudness,
                force_measurement.clone(),
            )
            .await?;
            if loudness_protocol_feature(&plan)
                .is_some_and(|feature| !executor_protocol.supports(feature))
            {
                // A track switch cannot move the session to a newer worker.
                // Drop a force-only encode rather than paying for unity gain;
                // an already-required encode remains playable without the
                // optional normalization fields its worker cannot understand.
                if force_measurement.is_some() {
                    sp = negotiate(false);
                    plan = sp.plan;
                    fill_audio_loudness_gains(
                        registry,
                        &session.parts,
                        &mut plan,
                        session.loudness,
                        force_measurement,
                    )
                    .await?;
                }
                if loudness_protocol_feature(&plan)
                    .is_some_and(|feature| !executor_protocol.supports(feature))
                {
                    apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                }
            }
            let verdict = replanned_verdict(&plan, &sp.video_verdict, &sp.audio_verdict)?;
            let mut subs = sp.subtitles;
            fill_verdict_track_ids(registry, &session.parts, &mut subs).await;
            *session.verdict.lock().unwrap() = Some(verdict);
            *session.sub_verdicts.lock().unwrap() = subs;
            let burns_ass =
                plan.burn_ass.is_some() || session.burn_ass_text.lock().unwrap().is_some();
            let (needs, _) = placement_need(&plan, &info, &session.parts, burns_ass);
            let mut plan_slot = session.plan.lock().unwrap();
            let mut needs_slot = session.needs.lock().unwrap();
            *plan_slot = Some(plan);
            *needs_slot = needs;
        }
        // Map the absolute position onto the right part (single-part
        // sessions: part 0, local == absolute).
        let idx = part_index(&session.parts, position_ms);
        let part = session
            .parts
            .get(idx)
            .context("session has no parts")?
            .clone();
        let local_ms = position_ms.saturating_sub(part.base_ms);
        session
            .current_part
            .store(idx, std::sync::atomic::Ordering::SeqCst);
        match &session.mode {
            Mode::Remux { run } => {
                // The old run ends first, evidence kept: a seek-restart's
                // new pipeline never shares a directory with the old one's
                // drain, but the old lease died with it either way, so a
                // fresh one is opened on whichever part the target lands in.
                // A seek restarts in the target part and spans the rest from
                // there: concat cannot serve the seek itself (it accepts one
                // and then plays from zero — measured), so the restart
                // stays, but it only ever happens for a seek now, never for
                // a boundary.
                let old = run.lock().unwrap().take();
                if let Some(old) = old {
                    let ended = old.end("hub-local worker").await;
                    if let Some(data_dir) = self.data_dir() {
                        let (item, header) = self.log_header(&session.id);
                        crate::sessionlog::store(
                            data_dir,
                            &item,
                            &session.id,
                            &format!("{header}{}", ended.bundle),
                        );
                    }
                    if let Some(multiple) = ended.pace {
                        self.fold_local_pace(registry, &session.pace_class, multiple)
                            .await;
                    }
                }
                let sink = session.sink.lock().unwrap().clone();
                let burn_sets = session.burn_sets.lock().unwrap().clone();
                let burn_ass = session.burn_ass_text.lock().unwrap().clone();
                let tail = self.open_part_leases(registry, &session.parts, idx).await?;
                let fresh = match self
                    .start_local(
                        &session.id,
                        plan,
                        // The session's own value, NOT a fresh
                        // computation: a seek re-plans, but the client
                        // keeps the playlist it already has and §6.2.1
                        // forbids the declaration moving under it.
                        session.target_duration_secs,
                        tail,
                        local_ms,
                        (!sink.is_empty()).then_some(sink.as_str()),
                        burn_sets.as_deref(),
                        burn_ass.as_deref(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(first)
                        if sink != "hlssink2"
                            && plan.segment_format == kahawai_media::remux::SegmentFormat::Ts =>
                    {
                        // The same TC-6 fallback the start path has: some
                        // content crashes hlssink3 on EVERY restart.
                        tracing::warn!(session = %session.id, error = format!("{first:#}"),
                            "seek restart failed; retrying with fallback sink");
                        let tail = self.open_part_leases(registry, &session.parts, idx).await?;
                        let r = self
                            .start_local(
                                &session.id,
                                plan,
                                session.target_duration_secs,
                                tail,
                                local_ms,
                                Some("hlssink2"),
                                burn_sets.as_deref(),
                                burn_ass.as_deref(),
                            )
                            .await
                            .with_context(|| format!("first attempt: {first:#}"))?;
                        *session.sink.lock().unwrap() = "hlssink2".into();
                        r
                    }
                    Err(e) => return Err(e),
                };
                fold_facts(&mut session.verdict.lock().unwrap(), &fresh.facts);
                self.watch_local_death(&session.id, fresh.run.died());
                *run.lock().unwrap() = Some(fresh.run);
                Ok(part.base_ms)
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let _ = registry
                    .send_to_tc(
                        &tc,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                kahawai_proto::v1::EndSession {
                                    session_id: session.id.clone(),
                                },
                            )),
                        },
                    )
                    .await;
                registry.tc_session_ended(&tc);
                // The hub-held lease survives restarts; reuse it while
                // the target stays inside the same part (works even when
                // the mediahost link is flapping). Crossing parts needs
                // a lease on the other file.
                let held = self.tc_leases.lock().unwrap().remove(&session.id);
                let parts = match held {
                    Some((parts, held_idx)) if held_idx == idx => parts,
                    _ => self.open_part_leases(registry, &session.parts, idx).await?,
                };
                let sink = session.sink.lock().unwrap().clone();
                // Read outside the call: a guard held across .await
                // would poison the future's Send-ness.
                let sets_bytes = session
                    .burn_sets
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|p| std::fs::read(p).unwrap_or_default())
                    .unwrap_or_default();
                let ass_bytes = session
                    .burn_ass_text
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
                    .into_bytes();
                if let Err(first) = self
                    .start_transcode(
                        registry,
                        &tc,
                        &session.id,
                        plan,
                        session.target_duration_secs,
                        parts,
                        idx,
                        local_ms,
                        &sink,
                        sets_bytes.clone(),
                        ass_bytes.clone(),
                    )
                    .await
                {
                    if sink == "hlssink2"
                        || plan.segment_format != kahawai_media::remux::SegmentFormat::Ts
                    {
                        return Err(first);
                    }
                    tracing::warn!(session = %session.id, error = format!("{first:#}"),
                        "seek restart failed; retrying with fallback sink");
                    let parts = self.open_part_leases(registry, &session.parts, idx).await?;
                    self.start_transcode(
                        registry,
                        &tc,
                        &session.id,
                        plan,
                        session.target_duration_secs,
                        parts,
                        idx,
                        local_ms,
                        "hlssink2",
                        sets_bytes,
                        ass_bytes,
                    )
                    .await
                    .with_context(|| format!("first attempt: {first:#}"))?;
                    *session.sink.lock().unwrap() = "hlssink2".into();
                }
                Ok(part.base_ms)
            }
            Mode::Direct { .. } => bail!("direct sessions seek with range requests"),
        }
    }
}
