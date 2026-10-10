
# Sourced, never run: point this shell at the patched GStreamer.
#
#   . "$(dirname "$0")/kahawai-gst-env.sh"
#
# Every script that builds or runs a kahawai pipeline ON THIS BOX has to
# do this, and the ones that did not were quietly testing a different
# GStreamer from the one that ships. `kahawai-sweep.sh` is the case that
# bit: it exists to validate the real remux pipeline before a release,
# and without the patched stack it validated the system plugins instead.
# Two files failed to demux under those and pass under ours — the AVI
# push-mode fixes in patches/gstreamer/0001 and 0002 — so the sweep was
# reporting failures the shipping stack does not have. It could as
# easily have hidden ones it does.
#
# The patched stack is the kahawai-gstreamer package (AUR): a whole
# GStreamer in /opt that nothing else on the box loads. patches/gstreamer/0004 changes the size
# of a public H.264 struct, so kahawai must be BUILT against that tree's
# headers, not only run against its libraries — hence PKG_CONFIG_PATH,
# which is why a script must source this before `cargo build`, not after.
# Its libgstreamer ignores GST_PLUGIN_PATH and GST_REGISTRY (the package's
# kahawai-isolate.patch) and finds its plugins beside itself, so the
# library path is all a kahawai process needs; PATH and GI_TYPELIB_PATH
# make a hand-run gst-launch, gst-inspect or reproducer use the same tree.
#
# Exported only into the shell that sources this, on purpose: in a login
# shell LD_LIBRARY_PATH would hand this GStreamer to every other program
# started from it.
#
# Missing is a WARNING, not an error: a box without the package can still
# run these scripts, it just is not answering for the shipping stack, and
# it should say so rather than look identical to one that is.
kahawai_gst=/opt/kahawai-gstreamer
if [ -d "$kahawai_gst/lib/pkgconfig" ]; then
  export PKG_CONFIG_PATH="$kahawai_gst/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  export LD_LIBRARY_PATH="$kahawai_gst/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  export GI_TYPELIB_PATH="$kahawai_gst/lib/girepository-1.0${GI_TYPELIB_PATH:+:$GI_TYPELIB_PATH}"
  export PATH="$kahawai_gst/bin:$PATH"
  echo "==> patched GStreamer: $kahawai_gst" >&2
else
  echo "==> WARNING: no patched GStreamer at $kahawai_gst" >&2
  echo "    using the system GStreamer, which is NOT what ships." >&2
  echo "    install the kahawai-gstreamer package (AUR)" >&2
fi
unset kahawai_gst
