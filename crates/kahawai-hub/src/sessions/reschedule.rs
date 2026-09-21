use super::*;

impl Sessions {
    /// A transcoder vanished (link loss): reschedule its sessions onto
    /// the remaining fleet at the viewer's last position (AR-6); end the
    /// ones nobody can take.
    pub async fn reschedule_for_transcoder(
        self: &Arc<Self>,
        registry: &Registry,
        transcoder: &str,
    ) -> (usize, usize) {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                matches!(&s.mode, Mode::Transcode { transcoder: t }
                    if *t.lock().unwrap() == transcoder)
            })
            .map(|s| s.id.clone())
            .collect();
        let (mut moved, mut ended) = (0, 0);
        for id in ids {
            match self.reschedule(registry, &id).await {
                Ok(new_tc) => {
                    tracing::info!(session = %id, from = transcoder, to = %new_tc, "session rescheduled");
                    moved += 1;
                }
                Err(e) => {
                    tracing::warn!(session = %id, error = format!("{e:#}"), "reschedule failed; ending");
                    self.end(&id).await;
                    ended += 1;
                }
            }
        }
        (moved, ended)
    }

    /// Re-dispatch one session (its transcoder died or its worker
    /// crashed) at the viewer's last reported position.
    /// The reservation `reserve_transcoder` takes below outlives
    /// several fallible steps — leases, a watch-state read — so the
    /// body is wrapped and the slot returned on any failure. A leaked
    /// one is invisible: the box simply looks busier than it is until
    /// the hub restarts, and two of them retire a `max_sessions = 2`
    /// box from the fleet.
    pub async fn reschedule(self: &Arc<Self>, registry: &Registry, id: &str) -> Result<String> {
        // Holds the reservation until the new box is actually running;
        // cleared on success so only failures give it back.
        let mut reserved: Option<String> = None;
        let out = self.reschedule_inner(registry, id, &mut reserved).await;
        if out.is_err()
            && let Some(tc) = reserved
        {
            registry.tc_session_ended(&tc);
        }
        out
    }

    pub(super) async fn reschedule_inner(
        self: &Arc<Self>,
        registry: &Registry,
        id: &str,
        reserved: &mut Option<String>,
    ) -> Result<String> {
        let session = self.get(id).context("no such session")?;
        let (plan, needs) = {
            let plan_slot = session.plan.lock().unwrap();
            let plan = (*plan_slot).context("not a pipeline session")?;
            let needs = session.needs.lock().unwrap().clone();
            (plan, needs)
        };
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a dispatched session");
        };
        let old_tc = transcoder.lock().unwrap().clone();
        registry.tc_session_ended(&old_tc);
        let new_tc = registry
            .reserve_transcoder(&needs)
            .context("no capable transcoder left")?;
        *reserved = Some(new_tc.clone());
        // Resume where the viewer was: the player posts progress every
        // 10 s, which is exactly the doc's start_offset for AR-6.
        let position_ms = session
            .last_position_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let idx = part_index(&session.parts, position_ms);
        let part = session
            .parts
            .get(idx)
            .context("session has no parts")?
            .clone();
        let local_ms = (position_ms).saturating_sub(part.base_ms);
        session
            .current_part
            .store(idx, std::sync::atomic::Ordering::SeqCst);
        // Reuse the hub-held lease when the position is still in its
        // part — the mediahost may be unreachable during a fleet blip.
        let held = self.tc_leases.lock().unwrap().remove(id);
        let parts = match held {
            Some((parts, held_idx)) if held_idx == idx => parts,
            _ => self.open_part_leases(registry, &session.parts, idx).await?,
        };
        let sets = session
            .burn_sets
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| std::fs::read(p).unwrap_or_default())
            .unwrap_or_default();
        let ass = session
            .burn_ass_text
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default()
            .into_bytes();
        self.start_transcode(
            registry,
            &new_tc,
            id,
            plan,
            session.target_duration_secs,
            parts,
            idx,
            local_ms,
            "",
            sets,
            ass,
        )
        .await?;
        *transcoder.lock().unwrap() = new_tc.clone();
        *reserved = None; // running now; the session owns the slot
        Ok(new_tc)
    }
}
