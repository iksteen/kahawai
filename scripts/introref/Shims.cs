// SPDX-License-Identifier: MIT
//
// The Jellyfin-shaped surface intro-skipper's analyzers reach for, and nothing
// else. Their analyzer sources are compiled unmodified against this, so the
// reference runs the plugin's real algorithms with no Jellyfin server, no
// Jellyfin assemblies, and no plugin database.
//
// Written for kahawai's comparison rig (docs/intro-detection-plan.md). The
// plugin's own sources stay where the rig cloned them; nothing GPL is vendored
// here.

using System.Collections.Concurrent;
using IntroSkipper.Configuration;
using IntroSkipper.Data;
using IntroSkipper.FFmpeg;
using MediaBrowser.Model.Entities;

namespace MediaBrowser.Model.Plugins
{
    /// <summary>Base class their PluginConfiguration derives from.</summary>
    public class BasePluginConfiguration
    {
    }
}

namespace MediaBrowser.Model.Entities
{
    /// <summary>A chapter marker. The rig has no chapter source, so the list is
    /// always empty — the chapter analyzer is out of scope on both sides.</summary>
    public class ChapterInfo
    {
        public long StartPositionTicks { get; set; }

        public string? Name { get; set; }
    }
}

namespace IntroSkipper
{
    /// <summary>
    /// Stands in for the Jellyfin plugin singleton: configuration, the ffmpeg
    /// binary, chapters (none), and the sink their analyzers write results to.
    /// </summary>
    public sealed class Plugin
    {
        public static Plugin? Instance { get; set; }

        public PluginConfiguration Configuration { get; set; } = new PluginConfiguration();

        public string FFmpegPath { get; set; } = "ffmpeg";

        /// <summary>Segments the analyzers reported, keyed by episode and mode.</summary>
        public ConcurrentDictionary<(Guid Episode, AnalysisMode Mode), Segment> Segments { get; } = new();

        public IReadOnlyList<ChapterInfo> GetChapters(Guid episodeId) => Array.Empty<ChapterInfo>();

        /// <summary>What an earlier analyzer already stored for this episode;
        /// recap detection reads it to bound its scan.</summary>
        internal static Task<IReadOnlyDictionary<AnalysisMode, Segment>> GetTimestampsAsync(
            Guid id,
            CancellationToken cancellationToken = default)
        {
            var found = Instance?.Segments
                .Where(kvp => kvp.Key.Episode == id)
                .ToDictionary(kvp => kvp.Key.Mode, kvp => kvp.Value)
                ?? new Dictionary<AnalysisMode, Segment>();
            return Task.FromResult<IReadOnlyDictionary<AnalysisMode, Segment>>(found);
        }

        public Task UpdateTimestampAsync(
            Segment segment,
            AnalysisMode mode,
            string? configHash = null,
            CancellationToken cancellationToken = default)
        {
            Segments[(segment.EpisodeId, mode)] = segment;
            return Task.CompletedTask;
        }
    }
}

namespace IntroSkipper.FFmpeg
{
    /// <summary>
    /// A cache that never hits. Every probe the rig runs is a real ffmpeg call,
    /// which is the point: the timings mean something and no earlier run can
    /// leak into a later one.
    /// </summary>
    public sealed class NullDetectionCache : IDetectionCacheService
    {
        public bool IsEnabled => false;

        public bool TryRead<T>(Guid itemId, AnalysisMode mode, CacheEntryType type, double start, double end, out T[] result)
        {
            result = Array.Empty<T>();
            return false;
        }

        public bool Write<T>(Guid itemId, AnalysisMode mode, CacheEntryType type, double start, double end, T[] items) => false;

        public void DeleteForItem(Guid itemId)
        {
        }

        public void DeleteByMode(AnalysisMode mode)
        {
        }

        public bool HasCachedFingerprint(QueuedEpisode episode, AnalysisMode mode) => false;
    }
}
