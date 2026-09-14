# Sourced, never run: the gst-plugins-rs half of the staged codec stack.
#
#   . "$(dirname "$0")/kahawai-gst-rs.sh"
#
# Three places build hlssink3 from source — this box through
# kahawai-gst-plugins.sh, the container through the Dockerfile, the mac
# through HomebrewFormula/kahawai-gstreamer.rb — and they must build
# the SAME thing. A constant copied into all of them is a constant that
# drifts, and a satellite quietly running a different sink from the hub is
# the class of bug this arrangement exists to prevent. So the tag, the
# unreleased list and the apply logic live here; the two that cannot
# source a shell file carry the same values and the same classification,
# and say so where they do.

# The gst-plugins-rs release to build hlssink3 from.
RS_TAG=gstreamer-1.28.7

# Patches in patches/gst-plugins-rs that NO gst-plugins-rs release carries
# yet. A system's hlssink3 therefore cannot have them however new it is,
# so a non-empty list means every box builds its own — a version check can
# only answer "does it have the RELEASED fixes".
#
# Both directions of this claim are verified against $RS_TAG in
# apply_rs_patches, so an entry that has landed upstream, or one missing
# that should be here, stops the build instead of drifting. Move an entry
# out of here when it lands in $RS_TAG.
RS_UNRELEASED="0002-hlssink3-EXTINF-must-be-the-distance-to-the-next-frag.patch"

# patches/gst-plugins-rs against an $RS_TAG checkout, conditionally: some
# of those patches are already upstream in the tag, and `git apply` refuses
# a patch that is already in the tree.
#
# So each one is classified rather than assumed. A patch that no longer
# applies for any OTHER reason stops the build: the tag has outgrown it and
# it needs rebasing, which is exactly the drift this exists to end.
#
# $1 the checkout, $2 the patches directory.
apply_rs_patches() {
    local rs="$1" dir="$2" p base unreleased
    for p in "$dir"/*.patch; do
        [ -e "$p" ] || continue
        base="$(basename "$p")"
        case " $RS_UNRELEASED " in *" $base "*) unreleased=1 ;; *) unreleased=0 ;; esac
        if git -C "$rs" apply --check "$p" 2>/dev/null; then
            [ "$unreleased" = 1 ] || die \
                "$base is not listed in RS_UNRELEASED but $RS_TAG does not carry it"
            echo "    applying $base" >&2
            git -C "$rs" apply "$p" || die "FAILED to apply $base to $RS_TAG"
        elif git -C "$rs" apply --reverse --check "$p" 2>/dev/null; then
            [ "$unreleased" = 0 ] || die \
                "$base is listed in RS_UNRELEASED but $RS_TAG already carries it — drop it from the list"
            echo "    already in $RS_TAG: $base" >&2
        else
            die "$base neither applies to nor is present in $RS_TAG — rebase it"
        fi
    done
}
