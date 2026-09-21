use super::*;

impl Sessions {
    /// Where diagnostics live: the scratch root is `<data_dir>/sessions`,
    /// so its parent is the data dir.
    /// OPS-10: the hub half of a session bundle — what only the hub
    /// knows. Structured state rather than log lines because the hub
    /// cannot read its own log: it writes to stdout, redirected by
    /// whatever launched it, and on macOS launchd discards it entirely.
    ///
    /// Returns `(item_id, header)`; the item id rides into the bundle
    /// FILENAME so the item detail page can find it later.
    pub fn log_header(&self, session_id: &str) -> (String, String) {
        use std::fmt::Write as _;
        let Some(s) = self.get(session_id) else {
            // Torn down already — which is the NORMAL case for a
            // dispatched session's bundle, since it crosses the link
            // after end() has forgotten the session.
            // NOT removed: a failed session emits a crash log AND a
            // bundle, and a mid-session death emits both too — each
            // needs the same header.
            if let Some(kept) = self.known_sessions.lock().unwrap().get(session_id) {
                return kept.clone();
            }
            return (
                "unknown".into(),
                format!("== hub: session {session_id}\n(session already ended; no hub state)\n\n"),
            );
        };
        let mut h = String::new();
        let _ = writeln!(h, "== hub: session {}", s.id);
        let _ = writeln!(h, "item:       {}", s.item_id);
        let _ = writeln!(h, "user:       {}", s.user_id);
        let _ = writeln!(
            h,
            "mode:       {}",
            match &s.mode {
                Mode::Direct { .. } => "direct",
                Mode::Remux { .. } => "remux (hub-local worker)",
                Mode::Transcode { .. } => "transcode (dispatched)",
            }
        );
        if let Mode::Transcode { transcoder } = &s.mode {
            let _ = writeln!(h, "placed on:  {}", transcoder.lock().unwrap());
        }
        if !s.pace_class.is_empty() {
            let _ = writeln!(h, "work class: {}", s.pace_class);
        }
        if let Some((video, audio)) = s.verdict.lock().unwrap().as_ref() {
            let _ = writeln!(h, "verdict:    v: {video}");
            let _ = writeln!(h, "            a: {audio}");
        }
        if let Some(plan) = *s.plan.lock().unwrap() {
            let _ = writeln!(h, "plan:       {plan:?}");
        }
        let _ = writeln!(h, "sink:       {}", s.sink.lock().unwrap());
        let _ = writeln!(h, "idle:       {}s", s.idle_for().as_secs());
        // An error recorded while the session was still live (a
        // mid-session death that AR-6 rescheduled) belongs here too.
        if let Some((_, kept)) = self.known_sessions.lock().unwrap().get(&s.id)
            && let Some(line) = kept.lines().find(|l| l.starts_with("error:"))
        {
            let _ = writeln!(h, "{line}");
        }
        let _ = writeln!(h);
        (s.item_id.clone(), h)
    }

    /// OPS-10: this session's diagnostics, for the download button.
    ///
    /// A LIVE dispatched session is asked over the link; a live local one
    /// is read straight off disk; an ENDED one comes from the bundle
    /// stored at teardown, which is the case that matters — nobody
    /// presses a button on a session they already closed.
    pub async fn collect_logs(
        &self,
        registry: &crate::registry::Registry,
        id: &str,
    ) -> Result<String> {
        let data_dir = self.data_dir().context("no data dir")?.to_path_buf();
        let Some(session) = self.get(id) else {
            // Ended: serve what teardown kept.
            let path = crate::sessionlog::for_session(&data_dir, id)
                .context("no logs kept for that session")?;
            return Ok(std::fs::read_to_string(path)?);
        };
        match &session.mode {
            Mode::Remux { run } => {
                let (_, header) = self.log_header(id);
                let bundle = run
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|run| run.bundle("hub-local worker"))
                    .unwrap_or_else(|| "== hub-local worker: restarting\n".into());
                let body = format!("{header}{bundle}");
                crate::sessionlog::store(&data_dir, &session.item_id, id, &body);
                Ok(crate::sessionlog::for_session(&data_dir, id)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or(body))
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.pending_logs.lock().unwrap().insert(id.into(), tx);
                let sent = registry
                    .send_to_tc(
                        &tc,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::CollectLogs(
                                kahawai_proto::v1::CollectLogs {
                                    session_id: id.to_string(),
                                },
                            )),
                        },
                    )
                    .await;
                if let Err(e) = sent {
                    self.pending_logs.lock().unwrap().remove(id);
                    // The box is gone; a stored bundle may still exist.
                    return match crate::sessionlog::for_session(&data_dir, id) {
                        Some(p) => Ok(std::fs::read_to_string(p)?),
                        // Typed, like the timeout below: "the transcoder is
                        // not reachable" and "this session kept no logs" are
                        // different answers and the API cannot tell them apart
                        // from a sentence.
                        // The transport failure is the SOURCE and the typed
                        // refusal is the context, not the other way round.
                        // Flattening the chain into the outermost layer is the
                        // exact shape `error.rs` names as a leak vector: it is
                        // harmless only while nothing reads this error's own
                        // `Display`, and it reads backwards in the log.
                        None => Err(e.context(SatelliteSilent(
                            "the transcoder running this session is not reachable".into(),
                        ))),
                    };
                }
                match tokio::time::timeout(Duration::from_secs(10), rx).await {
                    Ok(Ok(body)) => Ok(body),
                    _ => {
                        self.pending_logs.lock().unwrap().remove(id);
                        bail!(SatelliteSilent(
                            "the transcoder running this session did not answer in time".into()
                        ))
                    }
                }
            }
            Mode::Direct { .. } => {
                let (_, header) = self.log_header(id);
                Ok(format!(
                    "{header}(direct play: no pipeline, no worker log)\n"
                ))
            }
        }
    }

    /// OPS-10: remember which item a session id belongs to, from the
    /// moment the id exists. Everything that can go wrong after this
    /// point — a failed start, a mid-session death, a normal teardown —
    /// produces diagnostics that must file under the right item.
    pub(super) fn note_session(&self, id: &str, item_id: &str) {
        let mut kept = self.known_sessions.lock().unwrap();
        // Bounded: these are small headers, and only the recent ones can
        // still have diagnostics arriving for them.
        if kept.len() > 64 {
            kept.clear();
        }
        kept.insert(
            id.to_string(),
            (
                item_id.to_string(),
                format!("== hub: session {id}\nitem:       {item_id}\n(session did not reach a running state)\n\n"),
            ),
        );
        if let Some(data_dir) = self.data_dir() {
            crate::sessionlog::store(data_dir, item_id, id, &kept[id].1);
        }
    }

    /// OPS-10: record why a session failed, on the header every later
    /// bundle carries.
    ///
    /// Both the error path and teardown write a bundle for the same
    /// session, and they collide on the filename — so the teardown one,
    /// which is richer but knows nothing about the failure, would erase
    /// the error message. Putting the error on the HEADER means whichever
    /// write lands last still carries it, and the worker log is not
    /// duplicated into two files to achieve that.
    pub fn note_error(&self, session_id: &str, error: &str) {
        let mut kept = self.known_sessions.lock().unwrap();
        if let Some((_, header)) = kept.get_mut(session_id)
            && !header.contains("error:")
        {
            header.push_str(&format!("error:      {error}\n\n"));
        }
    }

    /// A bundle arrived for a caller waiting on it (the download button).
    pub fn deliver_logs(&self, session_id: &str, body: String) {
        if let Some(tx) = self.pending_logs.lock().unwrap().remove(session_id) {
            let _ = tx.send(body);
        }
    }

    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.scratch_root.parent()
    }
}

/// Hub restart: every run directory still under the scratch root belonged
/// to a session that was interrupted. Bundle each before the executor
/// sweeps them, keyed by the item the durable start header names.
pub(super) fn recover_interrupted_runs(scratch_root: &std::path::Path) {
    let Some(data_dir) = scratch_root.parent() else {
        return;
    };
    let Ok(sessions) = std::fs::read_dir(scratch_root) else {
        return;
    };
    for entry in sessions.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let Some(item) = crate::sessionlog::for_session(data_dir, &id)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|header| {
                header
                    .lines()
                    .find_map(|line| line.strip_prefix("item:").map(|s| s.trim().to_string()))
            })
        else {
            continue;
        };
        // Per-run directories `r<N>`, in order; a directory holding files
        // directly is a run from before per-run layout and is bundled as is.
        let mut runs: Vec<std::path::PathBuf> = std::fs::read_dir(entry.path())
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        runs.sort();
        if runs.is_empty() {
            runs.push(entry.path());
        }
        let mut body = String::from("== recovered after hub restart\n");
        for run in runs {
            body.push_str(&kahawai_playback::bundle::gather(
                "hub-local worker",
                &id,
                &run,
            ));
            body.push('\n');
        }
        crate::sessionlog::store(data_dir, &item, &id, &body);
    }
}
