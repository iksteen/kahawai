# Kahawai on macOS, built from source against the patched GStreamer.
#
#   brew tap iksteen/kahawai https://github.com/iksteen/kahawai
#   brew install --HEAD iksteen/kahawai/kahawai
#
# HEAD-only on purpose. The supported artifact is the container image;
# macOS is a build-from-source platform, and there is no stable release
# for a url/sha256 to point at yet. Add a `stable do` block here when
# there is.
#
# The GStreamer dependency is OURS, not Homebrew's. kahawai-gstreamer is
# the same upstream release with patches/ applied, keg-only so it cannot
# shadow the stock formula — see that recipe for why the whole stack is
# patched rather than a few plugins staged beside it. Because it is
# keg-only, nothing finds it unless pointed at it, which is what the
# PKG_CONFIG_PATH below does. A build that misses it links the unpatched
# GStreamer and every patch is silently absent.
class Kahawai < Formula
  desc "Self-hosted media streaming server"
  homepage "https://github.com/iksteen/kahawai"
  license "MIT"

  head "https://github.com/iksteen/kahawai.git", branch: "master"

  depends_on "cmake" => :build
  # The web UI is embedded in the hub binary and built by build.rs, which
  # runs npm itself. KAHAWAI_REQUIRE_WEB below turns a missing bundle
  # into a build failure rather than a server that serves no interface.
  depends_on "node" => :build
  depends_on "pkgconf" => :build
  depends_on "protobuf" => :build
  depends_on "rust" => :build

  depends_on "kahawai-gstreamer"
  depends_on "libass"
  # The OCR tier for bitmap subtitles (PGS, VobSub) is a default feature,
  # and leptess links both of these.
  depends_on "leptonica"
  depends_on "tesseract"

  def install
    ENV["KAHAWAI_REQUIRE_WEB"] = "1"
    ENV.prepend_path "PKG_CONFIG_PATH", Formula["kahawai-gstreamer"].opt_lib/"pkgconfig"

    # The UI's dependencies, before cargo. kahawai-hub's build script only
    # builds the web bundle when web/node_modules is already there, and a
    # fresh checkout has neither that nor web/dist — both are gitignored.
    # So without this the script silently skips the bundle and then panics
    # on the KAHAWAI_REQUIRE_WEB check above. Reproduced from a clean
    # clone: exit 101 at build.rs, "web/dist/index.html is missing".
    system "npm", "ci", "--prefix", "web"

    # One invocation, so the workspace's shared dependencies compile once
    # rather than once per binary. Four binaries come out of these three
    # packages: the everything binary, and a standalone each for hub,
    # mediahost and transcoder.
    system "cargo", "build", "--release", "--locked",
           "-p", "kahawai", "-p", "kahawai-mediahostd", "-p", "kahawai-transcoderd"
    bin.install %w[kahawai kahawai-hub kahawai-mediahost kahawai-transcoder]
                .map { |b| "target/release/#{b}" }

    # Kahawai's own default is XDG, which resolves inside whoever's home
    # the process happens to have. A service has no business there, so the
    # config names its directories outright. Generated rather than copied
    # from the repo because the paths it has to name are this prefix's.
    #
    # install, not write: pkgetc carries InstallRenamed, which leaves an
    # edited config alone and drops the new one beside it as
    # kahawai.toml.default. Writing it directly would either clobber the
    # user's file or, guarded, never tell them the default had moved.
    (buildpath/"kahawai.toml").write(default_config)
    pkgetc.install "kahawai.toml"
    %w[kahawai kahawai-mediahost kahawai-transcoder].each { |d| (var/d).mkpath }
    (var/"log").mkpath

    doc.install "README.md", "LICENSE", "docs"
  end

  # Enough to start, and nothing that pretends to know the machine. No
  # collections: an empty list is a hub with nothing in it, which is
  # obvious, where an invented /Users/Shared/Movies would be a hub that
  # looks configured and silently indexes nothing.
  def default_config
    <<~TOML
      # Kahawai, as installed by Homebrew. Edited in place; `brew upgrade`
      # leaves it alone, and a changed default arrives beside it as
      # kahawai.toml.default. Every key here has one — see
      # #{opt_share}/doc/#{name}/docs/kahawai-deployment.md — these are
      # set because a service must not depend on whose HOME it has.

      [all_in_one]
      transcoder = true

      [hub]
      bind = "127.0.0.1:8420"
      satellite_bind = "0.0.0.0:8421"
      data_dir = "#{var}/kahawai"

      [mediahost]
      name = "local"
      state_dir = "#{var}/kahawai-mediahost"

      # Add one of these per library. Roots are read and never written.
      # [[mediahost.collections]]
      # name = "movies"
      # media_type = "movies"
      # roots = ["/Volumes/media/movies"]

      [transcoder]
      hub = "127.0.0.1:8421"
      name = "local"
      state_dir = "#{var}/kahawai-transcoder"
    TOML
  end

  # The one service, and the only way this formula offers to run Kahawai.
  #
  # all-in-one, because Homebrew builds exactly one Homebrew::Service per
  # formula and `brew services` enumerates formulae rather than services:
  # a satellite role managed the same way would have to be its own
  # formula. Shipping unmanaged plists beside this would be a second way
  # to run the same software, which is worse than not offering one.
  service do
    # etc, not pkgetc: Homebrew::Service delegates a fixed list to the
    # formula (bin, etc, libexec, opt_*, var) and pkgetc is not on it.
    run [opt_bin/"kahawai", "--config", etc/"kahawai/kahawai.toml", "all-in-one"]
    keep_alive true
    working_dir var
    log_path var/"log/kahawai.log"
    error_log_path var/"log/kahawai.log"
  end

  def caveats
    <<~EOS
      Config, edited in place. An upgrade leaves your version alone and
      writes any new default beside it as kahawai.toml.default:
        #{pkgetc}/kahawai.toml
      It has no collections yet, so add some before starting anything.
      What the environment can and cannot do:
        kahawai doctor

      All-in-one, which is what this formula runs:
        brew services start kahawai

      Without sudo that is a USER AGENT: it starts at login rather than at
      boot, and macOS asks it for Local Network permission. `sudo brew
      services start kahawai` installs a system daemon instead, which is
      auto-approved (TN3179) and starts at boot.

      Either way the binary is signed by the build, so every upgrade
      re-signs it and the Local Network grant is asked for again.
      scripts/kahawai-mac.sh signs with a stable keychain identity, which
      is the only way to keep that grant across rebuilds.

      Satellite roles are not services here — a formula can only have one.
      The binaries are installed and take --config; supervise them with
      that script.

      Kahawai links the patched GStreamer in
        #{Formula["kahawai-gstreamer"].opt_prefix}
      not Homebrew's. `brew upgrade gstreamer` does not touch it, and
      upgrading THIS keg needs kahawai rebuilt: cargo caches the
      version-stamped Cellar path and the next build fails in the linker.
    EOS
  end

  test do
    assert_match "kahawai", shell_output("#{bin}/kahawai --version")

    # The config has to be one Kahawai ACCEPTS, not merely one that
    # exists: `deny_unknown_fields` means a key that drifted out of the
    # config struct is a startup failure in a service nobody is watching.
    # doctor loads the config before it checks anything, so its complaint
    # separates the two; whether this machine passes doctor is its own
    # business and not asserted here.
    loaded = `#{bin}/kahawai --config #{pkgetc}/kahawai.toml doctor 2>&1`
    refute_match(/unknown field|missing field|TOML parse error/, loaded)

    # The point of the whole arrangement: that the binary links OUR
    # GStreamer and not the stock one. Nothing else here would notice if
    # PKG_CONFIG_PATH had been wrong.
    keg = Formula["kahawai-gstreamer"].opt_prefix.realpath.to_s
    linked = shell_output("otool -L #{bin}/kahawai").lines.grep(/libgst/)
    refute_empty linked, "kahawai links no GStreamer at all"
    linked.each do |line|
      assert_match keg, line.strip, "links a GStreamer outside the patched keg"
    end
  end
end
