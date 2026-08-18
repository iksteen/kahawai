// SPDX-License-Identifier: MIT
//
// The reference side of the comparison: intro-skipper's own analyzers, driven
// over plain files.
//
//   introref segments [--anime] <file...>       segments as JSON, as `kahawai intro --json`
//   introref fingerprint <file> <start> <end>   their raw Chromaprint points, one per line
//   introref compare <lhs> <rhs>                their search over points from two files
//   introref silence <file> <start> <end>       their silencedetect ranges, one per line
//   introref keyframes <file> <start> <end>     their keyframe timestamps, one per line
//
// `compare` is what makes the port falsifiable: both implementations get the
// *same* fingerprints, so a difference in the output is a difference in the
// search and nowhere else.

using System.Diagnostics;
using System.Globalization;
using System.Text.Json;
using IntroSkipper;
using IntroSkipper.Analyzers;
using IntroSkipper.Configuration;
using IntroSkipper.Data;
using IntroSkipper.FFmpeg;
using Microsoft.Extensions.Logging.Abstractions;

var command = args.Length > 0 ? args[0] : "help";
var flags = new[] { "--anime", "--no-refine" };
var rest = args.Skip(1).Where(a => !flags.Contains(a)).ToArray();
var anime = args.Contains("--anime");

// --no-refine turns off everything that moves a boundary after the search, on
// both sides of the comparison: it separates a difference in the match from a
// difference in what each implementation does with it.
var configuration = new PluginConfiguration();
if (args.Contains("--no-refine"))
{
    configuration.AdjustIntroBasedOnSilence = false;
    configuration.SnapToKeyframe = false;
    configuration.AdjustIntroBasedOnChapters = false;
}

Plugin.Instance = new Plugin
{
    FFmpegPath = Environment.GetEnvironmentVariable("INTROREF_FFMPEG") ?? "ffmpeg",
    Configuration = configuration,
};

var ffmpeg = new FFmpegService(NullLogger<FFmpegService>.Instance, new NullDetectionCache());

switch (command)
{
    case "segments":
        await Segments(rest, anime).ConfigureAwait(false);
        break;
    case "fingerprint":
        await Fingerprint(rest).ConfigureAwait(false);
        break;
    case "compare":
        Compare(rest);
        break;
    case "silence":
        await Silence(rest).ConfigureAwait(false);
        break;
    case "keyframes":
        await Keyframes(rest).ConfigureAwait(false);
        break;
    default:
        Console.Error.WriteLine("usage: introref segments|fingerprint|compare|silence|keyframes ...");
        return 2;
}

return 0;

async Task Segments(string[] files, bool isAnime)
{
    var config = Plugin.Instance!.Configuration;
    var queue = new List<QueuedEpisode>();
    foreach (var file in files)
    {
        var duration = await ProbeDuration(file).ConfigureAwait(false);

        // QueueManager.cs: a quarter of anything at least five minutes long,
        // capped at AnalysisLengthLimit minutes; credits get their own tail.
        var introEnd = Math.Min(
            duration >= 5 * 60 ? duration * (config.AnalysisPercent / 100.0) : duration,
            60 * config.AnalysisLengthLimit);
        var maxCredits = Math.Min(duration, config.MaximumCreditsDuration);

        queue.Add(new QueuedEpisode
        {
            EpisodeId = Guid.NewGuid(),
            Name = Path.GetFileName(file),
            Path = file,
            Duration = duration,
            EpisodeNumber = queue.Count + 1,
            IntroFingerprintEnd = introEnd,
            CreditsFingerprintStart = Math.Max(0, duration - maxCredits),
            CreditsFingerprintEnd = duration,
        });
    }

    var started = Stopwatch.StartNew();
    // A ChromaprintAnalyzer per mode, as BaseItemAnalyzerTask builds one: the
    // analyzer caches inverted fingerprint indexes by episode id alone, so an
    // instance reused across modes would search the credits with the intro's
    // index. That is what happens if you hold one — the credits then come out
    // of the black-frame fallback, or not at all.
    var blackFrame = new BlackFrameAnalyzer(NullLogger<BlackFrameAnalyzer>.Instance, ffmpeg);
    ChromaprintAnalyzer Chromaprint() =>
        new(NullLogger<ChromaprintAnalyzer>.Instance, ffmpeg, new NullDetectionCache());

    await Chromaprint().AnalyzeMediaFiles(queue, AnalysisMode.Introduction, CancellationToken.None).ConfigureAwait(false);
    // Recap: their chromaprint analyzer in Recap mode, which finds the shared
    // card and then walks black frames out to the recap's end. The chapter
    // analyzer that would otherwise run first has nothing to read here.
    await Chromaprint().AnalyzeMediaFiles(queue, AnalysisMode.Recap, CancellationToken.None).ConfigureAwait(false);

    // BaseItemAnalyzerTask.cs: anime matches credits by fingerprint first,
    // everything else hunts black frames first. Each analyzer skips the
    // episodes an earlier one already answered.
    if (isAnime)
    {
        await Chromaprint().AnalyzeMediaFiles(queue, AnalysisMode.Credits, CancellationToken.None).ConfigureAwait(false);
        await blackFrame.AnalyzeMediaFiles(queue, AnalysisMode.Credits, CancellationToken.None).ConfigureAwait(false);
    }
    else
    {
        await blackFrame.AnalyzeMediaFiles(queue, AnalysisMode.Credits, CancellationToken.None).ConfigureAwait(false);
        await Chromaprint().AnalyzeMediaFiles(queue, AnalysisMode.Credits, CancellationToken.None).ConfigureAwait(false);
    }
    started.Stop();

    var episodes = queue.Select(e =>
    {
        var found = Plugin.Instance!.Segments;
        found.TryGetValue((e.EpisodeId, AnalysisMode.Introduction), out var intro);
        found.TryGetValue((e.EpisodeId, AnalysisMode.Recap), out var recap);
        found.TryGetValue((e.EpisodeId, AnalysisMode.Credits), out var credits);
        return new
        {
            name = e.Name,
            path = e.Path,
            duration = e.Duration,
            recap = Range(recap),
            intro = Range(intro),
            credits = Range(credits),
        };
    });

    Console.WriteLine(JsonSerializer.Serialize(
        new { episodes, seconds = started.Elapsed.TotalSeconds },
        new JsonSerializerOptions { WriteIndented = true }));
}

static object? Range(Segment? segment) =>
    segment is null || !segment.Valid ? null : new { start = segment.Start, end = segment.End };

async Task Fingerprint(string[] argv)
{
    var start = double.Parse(argv[1], CultureInfo.InvariantCulture);
    var end = double.Parse(argv[2], CultureInfo.InvariantCulture);
    var episode = new QueuedEpisode
    {
        EpisodeId = Guid.NewGuid(),
        Path = argv[0],
        IntroFingerprintEnd = end,
        CreditsFingerprintStart = start,
        CreditsFingerprintEnd = end,
    };
    // A window starting at zero is an intro; anything else is only reachable
    // through their credits mode, which is where the offset lives.
    var mode = start > 0 ? AnalysisMode.Credits : AnalysisMode.Introduction;
    var points = await ffmpeg.FingerprintAsync(episode, mode, CancellationToken.None).ConfigureAwait(false);
    Console.WriteLine(string.Join('\n', points));
}

// Their silencedetect wrapper over one window, so the two silence detectors can
// be compared directly instead of only through the boundary they move.
async Task Silence(string[] argv)
{
    var start = double.Parse(argv[1], CultureInfo.InvariantCulture);
    var end = double.Parse(argv[2], CultureInfo.InvariantCulture);
    var episode = new QueuedEpisode { EpisodeId = Guid.NewGuid(), Path = argv[0] };
    var ranges = await ffmpeg.DetectSilenceAsync(
        episode,
        new TimeRange(start, end),
        AnalysisMode.Introduction,
        CancellationToken.None).ConfigureAwait(false);
    foreach (var range in ranges)
    {
        Console.WriteLine(string.Create(CultureInfo.InvariantCulture, $"{range.Start:F3} {range.End:F3}"));
    }
}

// Their keyframe scan over one window: the other half of the end refinement.
async Task Keyframes(string[] argv)
{
    var start = double.Parse(argv[1], CultureInfo.InvariantCulture);
    var end = double.Parse(argv[2], CultureInfo.InvariantCulture);
    var episode = new QueuedEpisode { EpisodeId = Guid.NewGuid(), Path = argv[0] };
    var times = await ffmpeg.DetectKeyFramesAsync(
        episode,
        new TimeRange(start, end),
        AnalysisMode.Introduction,
        CancellationToken.None).ConfigureAwait(false);
    foreach (var time in times)
    {
        Console.WriteLine(string.Create(CultureInfo.InvariantCulture, $"{time:F3}"));
    }
}

void Compare(string[] argv)
{
    var lhs = ReadPoints(argv[0]);
    var rhs = ReadPoints(argv[1]);
    var analyzer = new ChromaprintAnalyzer(NullLogger<ChromaprintAnalyzer>.Instance, null!, null!);
    var (left, right) = analyzer.CompareEpisodes(Guid.NewGuid(), lhs, Guid.NewGuid(), rhs);
    Console.WriteLine(JsonSerializer.Serialize(new { lhs = Range(left), rhs = Range(right) }));
}

static uint[] ReadPoints(string path) => File.ReadAllLines(path)
    .Where(l => !string.IsNullOrWhiteSpace(l))
    .Select(l => uint.Parse(l.Trim(), CultureInfo.InvariantCulture))
    .ToArray();

async Task<double> ProbeDuration(string path)
{
    var ffprobe = Path.Combine(
        Path.GetDirectoryName(Plugin.Instance!.FFmpegPath) ?? string.Empty,
        "ffprobe");
    var info = new ProcessStartInfo(ffprobe)
    {
        RedirectStandardOutput = true,
        UseShellExecute = false,
    };
    foreach (var arg in new[] { "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path })
    {
        info.ArgumentList.Add(arg);
    }

    using var process = Process.Start(info) ?? throw new InvalidOperationException("ffprobe did not start");
    var output = await process.StandardOutput.ReadToEndAsync().ConfigureAwait(false);
    await process.WaitForExitAsync().ConfigureAwait(false);
    return double.Parse(output.Trim(), CultureInfo.InvariantCulture);
}
