# Sourced, never run: point this shell at the locally built GStreamer.
#
#   . "$(dirname "$0")/kahawai-gst-env.sh"
#
# Every script that runs a kahawai pipeline ON THIS BOX has to do this,
# and the ones that did not were quietly testing a different GStreamer
# from the one that ships. `kahawai-sweep.sh` is the case that bit:
# it exists to validate the real remux pipeline before a release, and
# without these two variables it validated the system plugins instead.
# Two files failed to demux under those and pass under ours — the AVI
# push-mode fixes in patches/gstreamer/0001 and 0002 — so the sweep was
# reporting failures the shipping stack does not have. It could as
# easily have hidden ones it does.
#
# patches/gstreamer/0004 changes the size of a public H.264 struct, so
# the plugins holding one and the library they hold it from must come
# from the same build and must not be reachable by anything built
# against the other ABI. Hence a kahawai-only directory that
# `kahawai-gst-plugins.sh` owns and wipes, invisible to every other
# GStreamer program on the box. The plugins carry an RPATH to the
# matching library; the path is exported anyway so a hand-run
# gst-launch against the same directory behaves like the service does.
#
# Missing is a WARNING, not an error: a box with no staged build can
# still run these scripts, it just is not answering for the shipping
# stack, and it should say so rather than look identical to one that is.
kahawai_gst="$HOME/.local/lib/kahawai-gst"
if [ -d "$kahawai_gst/plugins" ]; then
  export GST_PLUGIN_PATH="$kahawai_gst/plugins${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
  export LD_LIBRARY_PATH="$kahawai_gst/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  echo "==> staged plugins: $kahawai_gst/plugins" >&2
else
  echo "==> WARNING: no patched plugins at $kahawai_gst/plugins" >&2
  echo "    using the system GStreamer, which is NOT what ships." >&2
  echo "    build them: scripts/kahawai-gst-plugins.sh build" >&2
  echo "    check them: scripts/kahawai-gst-plugins.sh verify" >&2
fi
unset kahawai_gst
