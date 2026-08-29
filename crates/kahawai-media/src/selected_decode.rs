//! One parsed elementary stream, selected before decoder autoplugging.
//!
//! `decodebin3` stream selection still keeps every stream in an internal
//! multiqueue. A secondary Matroska Opus track with mostly missing timestamps
//! then stopped delivering after ~34 content minutes while the demuxer kept
//! synchronizing unselected streams. This primitive instead demuxes/parses
//! first, selects one pad by kind/index, and gives only that pad to a plain
//! decodebin. Unselected parsed pads remain unlinked and are never decoded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Audio,
    Video,
}

impl StreamKind {
    fn matches(self, caps: &gst::CapsRef) -> bool {
        let Some(name) = caps.structure(0).map(|structure| structure.name()) else {
            return false;
        };
        match self {
            Self::Audio => name.starts_with("audio/"),
            Self::Video => name.starts_with("video/") || name.starts_with("image/"),
        }
    }
}

/// A parsebin plus a decoder for one exact elementary-stream pad.
pub struct SelectedDecode {
    parser: gst::Element,
    decoder: gst::Element,
    kind: StreamKind,
    index: usize,
    selected_input: Arc<AtomicBool>,
    output: Arc<AtomicBool>,
    missing: Arc<AtomicBool>,
}

impl SelectedDecode {
    pub fn new(kind: StreamKind, index: usize, stop_at: Option<gst::Caps>) -> Result<Self> {
        crate::init()?;
        let parser = gst::ElementFactory::make("parsebin")
            .build()
            .context("parsebin")?;
        let mut builder = gst::ElementFactory::make("decodebin");
        if let Some(caps) = stop_at {
            builder = builder.property("caps", caps);
        }
        Ok(Self {
            parser,
            decoder: builder.build().context("decodebin")?,
            kind,
            index,
            selected_input: Arc::new(AtomicBool::new(false)),
            output: Arc::new(AtomicBool::new(false)),
            missing: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn output_flag(&self) -> Arc<AtomicBool> {
        self.output.clone()
    }

    pub fn missing_flag(&self) -> Arc<AtomicBool> {
        self.missing.clone()
    }

    /// Add source, parser and selected decoder to `pipeline`, and link the
    /// decoder's sole output to `target`.
    pub fn install(
        &self,
        pipeline: &gst::Pipeline,
        source: &gst::Element,
        target: &gst::Pad,
    ) -> Result<()> {
        pipeline.add_many([source, &self.parser, &self.decoder])?;
        source.link(&self.parser)?;

        let target = target.clone();
        let output = self.output.clone();
        self.decoder.connect_pad_added(move |_, pad| {
            if target.is_linked() {
                return;
            }
            if pad.link(&target).is_ok() {
                output.store(true, Ordering::Release);
            }
        });

        let seen = Arc::new(AtomicUsize::new(0));
        let selected_input = self.selected_input.clone();
        let decoder_sink = self
            .decoder
            .static_pad("sink")
            .expect("decodebin has a sink");
        let kind = self.kind;
        let index = self.index;
        self.parser.connect_pad_added(move |_, pad| {
            let caps = pad
                .stream()
                .and_then(|stream| stream.caps())
                .or_else(|| pad.current_caps());
            let wanted = caps.as_ref().is_some_and(|caps| kind.matches(caps));
            let selected = wanted && seen.fetch_add(1, Ordering::Relaxed) == index;
            if selected && pad.link(&decoder_sink).is_ok() {
                selected_input.store(true, Ordering::Release);
            }
        });

        let weak_pipeline = pipeline.downgrade();
        let selected_input = self.selected_input.clone();
        let missing = self.missing.clone();
        self.parser.connect_no_more_pads(move |parser| {
            if selected_input.load(Ordering::Acquire) {
                return;
            }
            missing.store(true, Ordering::Release);
            if let Some(pipeline) = weak_pipeline.upgrade() {
                let structure = gst::Structure::builder("kahawai-selected-decode-missing").build();
                let message = gst::message::Application::builder(structure)
                    .src(parser)
                    .build();
                let _ = pipeline.post_message(message);
            }
        });

        let weak_pipeline = pipeline.downgrade();
        let selected_input = self.selected_input.clone();
        let output = self.output.clone();
        let missing = self.missing.clone();
        self.decoder.connect_no_more_pads(move |decoder| {
            if !selected_input.load(Ordering::Acquire) || output.load(Ordering::Acquire) {
                return;
            }
            missing.store(true, Ordering::Release);
            if let Some(pipeline) = weak_pipeline.upgrade() {
                let structure = gst::Structure::builder("kahawai-selected-decode-missing").build();
                let message = gst::message::Application::builder(structure)
                    .src(decoder)
                    .build();
                let _ = pipeline.post_message(message);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_the_requested_media_kind() {
        gst::init().unwrap();
        let audio = gst::Caps::builder("audio/x-opus").build();
        let video = gst::Caps::builder("video/x-av1").build();
        let image = gst::Caps::builder("image/jpeg").build();
        assert!(StreamKind::Audio.matches(&audio));
        assert!(!StreamKind::Audio.matches(&video));
        assert!(StreamKind::Video.matches(&video));
        assert!(StreamKind::Video.matches(&image));
        assert!(!StreamKind::Video.matches(&audio));
    }

    #[test]
    fn selecting_audio_builds_one_decoder_without_a_cross_stream_queue() {
        if !crate::testutil::require_h264_aac_fixture() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video-and-audio.mkv");
        crate::testutil::render_h264_aac_mkv(&path);

        let pipeline = gst::Pipeline::new();
        let factories: Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let found = factories.clone();
        pipeline.connect_deep_element_added(move |_, _, element| {
            let Some(factory) = element.factory() else {
                return;
            };
            found.lock().unwrap().push((
                factory.name().to_string(),
                factory
                    .metadata("klass")
                    .map_or_else(String::new, |klass| klass.to_string()),
            ));
        });
        let source = gst::ElementFactory::make("filesrc")
            .property("location", &path)
            .build()
            .unwrap();
        let decode = SelectedDecode::new(
            StreamKind::Audio,
            0,
            Some(gst::Caps::builder("audio/x-raw").build()),
        )
        .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        decode
            .install(&pipeline, &source, &sink.static_pad("sink").unwrap())
            .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));
        assert!(decode.output_flag().load(Ordering::Acquire));

        let factories = factories.lock().unwrap();
        assert_eq!(
            factories
                .iter()
                .filter(|(_, class)| class.contains("Decoder/Audio"))
                .count(),
            1
        );
        assert!(
            factories
                .iter()
                .all(|(_, class)| !class.contains("Decoder/Video"))
        );
        assert!(
            factories.iter().all(|(name, _)| name != "multiqueue"),
            "selected and unselected streams must not share a synchronization queue"
        );
    }
}
