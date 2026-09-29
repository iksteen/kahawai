/* macOS live regression for vtenc rate-control policy. No external media.
 * Build/run with the companion Python script. The installed plugin should fail;
 * the patched plugin must retain quality control AND obey bitrate controls.
 */
#include <gst/gst.h>
#include <gst/app/gstappsink.h>
#include <stdio.h>

#define FPS 24
#define FRAMES 240

typedef struct {
  guint bitrate;
  gdouble quality;
  gint mode;
  const gchar *limit;
  const gchar *name;
} Phase;

static const Phase phases[] = {
  {1500, .25, 0, "0,0", "abr-low"},
  {6000, .25, 0, "0,0", "abr-high"},
  {6000, .75, 0, "0,0", "abr-quality-ignored"},
  {0, .25, 0, "0,0", "quality-low"},
  {0, .75, 0, "0,0", "quality-high"},
  {1500, .75, 0, "0,0", "quality-to-abr"},
  {1500, .75, 1, "0,0", "abr-to-cbr"},
  {1500, .75, 0, "0,0", "cbr-to-abr"},
  {0, .75, 0, "1500,1", "limit-only"},
  {0, .75, 0, "0,0", "limit-to-quality"},
  {6000, .75, 0, "1500,1", "abr-with-limit"},
  {6000, .75, 0, "0,0", "remove-limit"},
};
#define N_PHASES G_N_ELEMENTS(phases)

typedef struct {
  GstElement *encoder;
  guint input;
} Context;

static void
set_phase (GstElement *encoder, guint i)
{
  const Phase *p = &phases[i];
  g_object_set (encoder, "bitrate", p->bitrate, "quality", p->quality,
      "rate-control", p->mode, "data-rate-limits", p->limit, NULL);
}

static GstPadProbeReturn
change_phase (GstPad *pad, GstPadProbeInfo *info, gpointer data)
{
  Context *c = data;
  (void) pad;
  (void) info;
  if (c->input % FRAMES == 0 && c->input / FRAMES < N_PHASES)
    set_phase (c->encoder, c->input / FRAMES);
  c->input++;
  return GST_PAD_PROBE_OK;
}

/* vtenc emits length-prefixed AVC/HEVC access units. Count picture-bearing
 * data separately: a CBR stream padded to the target is not a quality pass. */
static guint64
payload_size (const guint8 *data, gsize size, gboolean hevc)
{
  guint64 total = 0;
  gsize pos = 0;
  while (pos + 4 <= size) {
    guint32 n = GST_READ_UINT32_BE (data + pos);
    pos += 4;
    g_assert_cmpuint (n, >, 0);
    g_assert_cmpuint (n, <=, size - pos);
    guint type = hevc ? (data[pos] >> 1) & 63 : data[pos] & 31;
    if (type != (hevc ? 38 : 12))
      total += n + 4;
    pos += n;
  }
  g_assert_cmpuint (pos, ==, size);
  return total;
}

static void
check_codec (gboolean hevc, const gchar *directory)
{
  const gchar *factory = hevc ? "vtenc_h265_hw" : "vtenc_h264_hw";
  gchar *description = g_strdup_printf (
      "videotestsrc num-buffers=%u pattern=smpte ! "
      "video/x-raw,format=NV12,width=1920,height=1080,framerate=24/1 ! "
      "%s name=enc max-keyframe-interval=48 ! tee name=t "
      "t. ! queue ! appsink name=out sync=false "
      "t. ! queue ! %sparse config-interval=-1 ! "
      "video/x-%s,stream-format=byte-stream,alignment=au ! "
      "filesink name=file sync=false",
      (guint) N_PHASES * FRAMES, factory, hevc ? "h265" : "h264",
      hevc ? "h265" : "h264");
  GError *error = NULL;
  GstElement *pipe = gst_parse_launch (description, &error);
  g_assert_no_error (error);
  g_free (description);
  GstElement *enc = gst_bin_get_by_name (GST_BIN (pipe), "enc");
  GstElement *sink = gst_bin_get_by_name (GST_BIN (pipe), "out");
  const gchar *plugin = gst_plugin_feature_get_plugin_name (GST_PLUGIN_FEATURE (
          gst_element_get_factory (enc)));
  GstPlugin *loaded = gst_registry_find_plugin (gst_registry_get (), plugin);
  g_print ("%s plugin: %s\n", factory, gst_plugin_get_filename (loaded));
  if (g_getenv ("VT_TEST_PLUGIN"))
    g_assert_cmpstr (gst_plugin_get_filename (loaded), ==,
        g_getenv ("VT_TEST_PLUGIN"));
  gst_object_unref (loaded);
  gchar *path = g_strdup_printf ("%s/%s.%s", directory, factory,
      hevc ? "hevc" : "h264");
  GstElement *file = gst_bin_get_by_name (GST_BIN (pipe), "file");
  g_object_set (file, "location", path, NULL);
  gst_object_unref (file);
  Context ctx = {enc, 0};
  set_phase (enc, 0);
  GstPad *pad = gst_element_get_static_pad (enc, "sink");
  gst_pad_add_probe (pad, GST_PAD_PROBE_TYPE_BUFFER, change_phase, &ctx, NULL);
  gst_object_unref (pad);
  guint counts[N_PHASES] = {0};
  guint64 bytes[N_PHASES] = {0}, picture[N_PHASES] = {0};
  GstBus *bus = gst_element_get_bus (pipe);
  g_assert_cmpint (gst_element_set_state (pipe, GST_STATE_PLAYING), !=,
      GST_STATE_CHANGE_FAILURE);
  gint64 deadline = g_get_monotonic_time () + 180 * G_TIME_SPAN_SECOND;
  for (;;) {
    GstSample *sample = gst_app_sink_try_pull_sample (GST_APP_SINK (sink), GST_SECOND);
    if (!sample) {
      GstMessage *msg = gst_bus_pop_filtered (bus, GST_MESSAGE_ERROR);
      if (msg) {
        gchar *debug;
        gst_message_parse_error (msg, &error, &debug);
        g_error ("%s: %s (%s)", factory, error->message, debug);
      }
      if (gst_app_sink_is_eos (GST_APP_SINK (sink)))
        break;
      g_assert_cmpint (g_get_monotonic_time (), <, deadline);
      continue;
    }
    GstBuffer *buffer = gst_sample_get_buffer (sample);
    GstClockTime timestamp = gst_segment_to_stream_time (
        gst_sample_get_segment (sample), GST_FORMAT_TIME, GST_BUFFER_PTS (buffer));
    g_assert_true (GST_CLOCK_TIME_IS_VALID (timestamp));
    guint frame = gst_util_uint64_scale_round (timestamp, FPS, GST_SECOND);
    guint phase = frame / FRAMES;
    g_assert_cmpuint (phase, <, N_PHASES);
    GstMapInfo map;
    g_assert_true (gst_buffer_map (buffer, &map, GST_MAP_READ));
    counts[phase]++;
    if (frame % FRAMES >= FPS) { /* one second for rate-control settling */
      bytes[phase] += map.size;
      picture[phase] += payload_size (map.data, map.size, hevc);
    }
    gst_buffer_unmap (buffer, &map);
    gst_sample_unref (sample);
  }
  GstMessage *done = gst_bus_timed_pop_filtered (bus, 30 * GST_SECOND,
      GST_MESSAGE_ERROR | GST_MESSAGE_EOS);
  g_assert_nonnull (done);
  g_assert_cmpint (GST_MESSAGE_TYPE (done), ==, GST_MESSAGE_EOS);
  gst_message_unref (done);
  gst_element_set_state (pipe, GST_STATE_NULL);
  gdouble rate[N_PHASES];
  for (guint i = 0; i < N_PHASES; i++) {
    g_assert_cmpuint (counts[i], ==, FRAMES);
    rate[i] = bytes[i] * 8.0 / (FRAMES / FPS - 1) / 1000;
    g_print ("%s %s: %.0f kbit/s, %.0f picture kbit/s, %u frames\n",
        factory, phases[i].name, rate[i],
        picture[i] * 8.0 / (FRAMES / FPS - 1) / 1000, counts[i]);
  }
  const guint low[] = {0, 5, 6, 7, 8, 10};
  for (guint i = 0; i < G_N_ELEMENTS (low); i++) {
    g_assert_cmpfloat (rate[low[i]], >, 750);
    g_assert_cmpfloat (rate[low[i]], <, 2250);
  }
  const guint high[] = {1, 2, 11};
  for (guint i = 0; i < G_N_ELEMENTS (high); i++) {
    g_assert_cmpfloat (rate[high[i]], >, 3900);
    g_assert_cmpfloat (rate[high[i]], <, 8400);
  }
  g_assert_cmpfloat (rate[4], >, rate[3] * 1.5);
  g_assert_cmpfloat (rate[9], >, rate[3] * 1.5);
  g_assert_cmpfloat (rate[2] / rate[1], >, .7);
  g_assert_cmpfloat (rate[2] / rate[1], <, 1.3);
  g_free (path);
  gst_object_unref (bus);
  gst_object_unref (sink);
  gst_object_unref (enc);
  gst_object_unref (pipe);
}

int
main (int argc, char **argv)
{
  gst_init (&argc, &argv);
  g_assert_cmpint (argc, ==, 2);
  check_codec (FALSE, argv[1]);
  check_codec (TRUE, argv[1]);
  return 0;
}
