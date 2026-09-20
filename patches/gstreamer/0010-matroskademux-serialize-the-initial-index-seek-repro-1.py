#!/usr/bin/env python3
"""Pin the initial index seek against natural Cues parsing.

The debug callback parks the streaming thread immediately before it parses
the trailing Cues. The application's first flush releases it. Without the
patch, READ_STATE_SEEK is already published and the chain starts a second
byte seek inside that flush: "Failed to seek". With the patch, the chain
stops before the application installs its pending seek.

Exit 0 requires a successful seek, the requested segment, and EOS. Missing
plugins, a missed rendezvous, and timeouts fail instead of skipping the test.
"""

import sys
import tempfile
import threading
from pathlib import Path

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402


def run():
    Gst.init(None)
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "in.mkv"
        fixture = Gst.parse_launch(
            "videotestsrc num-buffers=250 ! "
            "video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! "
            "x264enc key-int-max=25 ! h264parse ! matroskamux ! "
            f'filesink location="{path}"'
        )
        try:
            fixture.set_state(Gst.State.PLAYING)
            message = fixture.get_bus().timed_pop_filtered(
                20 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR
            )
            assert message and message.type == Gst.MessageType.EOS, "fixture failed"
        finally:
            fixture.set_state(Gst.State.NULL)
        data = path.read_bytes()
        pipeline = Gst.parse_launch(
            "appsrc name=source stream-type=seekable format=bytes ! "
            "matroskademux name=demux ! fakesink name=sink sync=false"
        )
        source = pipeline.get_by_name("source")
        demux = pipeline.get_by_name("demux")
        source.set_property("size", len(data))
        position = 0

        def need_data(source, _length):
            nonlocal position
            offset = position
            block = data[offset:offset + 65536]
            position += len(block)
            if block:
                buffer = Gst.Buffer.new_wrapped(block)
                buffer.offset = offset
                source.emit("push-buffer", buffer)
            else:
                source.emit("end-of-stream")

        def seek_data(_source, offset):
            nonlocal position
            position = offset
            return True

        source.connect("need-data", need_data)
        source.connect("seek-data", seek_data)
        at_cues = threading.Event()
        flushed = threading.Event()
        failures = []
        segments = []

        def log(_category, _level, _file, function, _line, _obj, message, *_rest):
            if (function == "gst_matroska_demux_chain"
                    and "Element id 0x1c53bb6b" in message.get()
                    and not at_cues.is_set()):
                at_cues.set()
                if not flushed.wait(5):
                    failures.append("initial seek did not flush the chain")

        def flushing(_pad, info):
            if info.get_event().type == Gst.EventType.FLUSH_START:
                flushed.set()
            return Gst.PadProbeReturn.OK

        def segment(_pad, info):
            event = info.get_event()
            if event.type == Gst.EventType.SEGMENT:
                segments.append(event.parse_segment().start)
            return Gst.PadProbeReturn.OK

        demux.get_static_pad("sink").add_probe(
            Gst.PadProbeType.EVENT_BOTH | Gst.PadProbeType.EVENT_FLUSH, flushing
        )
        pipeline.get_by_name("sink").get_static_pad("sink").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM, segment
        )
        Gst.debug_set_threshold_for_name("matroskademux", Gst.DebugLevel.LOG)
        Gst.debug_remove_log_function(None)
        Gst.debug_add_log_function(log, None)
        try:
            pipeline.set_state(Gst.State.PLAYING)
            assert at_cues.wait(10), "streaming did not reach trailing Cues"
            accepted = demux.srcpads[0].send_event(Gst.Event.new_seek(
                1.0, Gst.Format.TIME, Gst.SeekFlags.FLUSH | Gst.SeekFlags.KEY_UNIT,
                Gst.SeekType.SET, 6 * Gst.SECOND, Gst.SeekType.NONE, -1,
            ))
            message = pipeline.get_bus().timed_pop_filtered(
                10 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR
            )
            if message and message.type == Gst.MessageType.ERROR:
                error, debug = message.parse_error()
                if debug and "Failed to seek" in debug:
                    print(f"AFFECTED: {error.message}: {debug}")
                    return 1
                raise RuntimeError((error, debug))
            assert accepted, "initial seek was rejected"
            assert message and message.type == Gst.MessageType.EOS, "no EOS after seek"
            assert not failures, failures
            assert segments[-1] == 6 * Gst.SECOND, segments
        finally:
            flushed.set()
            pipeline.set_state(Gst.State.NULL)
            Gst.debug_remove_log_function(log)
    print("PASS: initial index seek is serialized with natural Cues parsing")
    return 0


if __name__ == "__main__":
    sys.exit(run())
