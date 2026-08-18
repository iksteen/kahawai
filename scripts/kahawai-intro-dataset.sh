#!/usr/bin/env bash
# Build a synthetic season with known intro and credits boundaries, so intro
# detection can be scored for *accuracy* and not just for agreement:
#
#   scripts/kahawai-intro-dataset.sh DEST [EPISODES]
#   scripts/kahawai-intro-compare.py l3 DEST
#
# Every episode is a shared "previously on" card over its own recap footage (of
# its own length), a black frame, the same 30 s opening (a melody over a title card), its own 8
# minute body, and the same 45 s of credits over black. The truth lands in
# DEST/labels.json, which the comparison script scores both implementations
# against.
#
# Deliberately not real television: the point of this dataset is that the
# answer is known. Real episodes go through the same comparison, without labels.
set -euo pipefail

DEST=${1:?usage: kahawai-intro-dataset.sh DEST [EPISODES]}
COUNT=${2:-6}
FFMPEG=${FFMPEG:-ffmpeg}

STING=5      # the shared "previously on" card
RECAP_BASE=20 # its episode-specific content, which grows per episode
BLACK=1      # the black frame a recap ends on
INTRO=30
BODY=480
CREDITS=45

mkdir -p "$DEST"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A melody, so the fingerprint has chroma to key on: a pure tone quantizes to
# an almost constant fingerprint and would match anything.
# The commas inside the expression are escaped: unescaped they would split the
# filtergraph instead of the function arguments.
melody() { # base_hz notes_per_second [semitone_step]
    step=${3:-1}
    echo "aevalsrc=0.4*sin(2*PI*t*($1*pow(2\,mod(floor(t*$2)*$step\,12)/12))):s=48000:c=stereo"
}

clip() { # out video_source audio_source seconds
    $FFMPEG -y -v error -f lavfi -i "$2" -f lavfi -i "$3" -t "$4" \
        -c:v libx264 -preset ultrafast -tune stillimage -pix_fmt yuv420p \
        -g 50 -r 10 -s 320x180 -c:a aac -ar 48000 -ac 2 "$1"
}

echo "building the shared card, opening and credits"
# The recap's card: shared audio, which is the only part of a recap that
# repeats. What follows it differs every episode and ends on a black frame.
clip "$work/sting.mkv" "color=c=maroon:s=320x180:r=10" "$(melody 330 3)" "$STING"
clip "$work/intro.mkv" "color=c=navy:s=320x180:r=10" "$(melody 220 2)" "$INTRO"
# Credits: black frames, which is the other signal being measured.
clip "$work/credits.mkv" "color=c=black:s=320x180:r=10" "$(melody 165 1)" "$CREDITS"

labels="{"
for i in $(seq 1 "$COUNT"); do
    printf 'episode %d/%d\n' "$i" "$COUNT"
    clip "$work/body.mkv" "testsrc2=s=320x180:r=10" "$(melody $((300 + i * 37)) $((2 + i % 3)))" "$BODY"
    # A different sequence of pitch classes per episode, not just a different
    # key: Chromaprint hears pitch classes, so a transposed melody matches its
    # original — and white noise, whose chroma is flat, matches everything.
    # Steps coprime to 12 walk the twelve notes in a different order each time.
    step=$(( (i % 4) * 2 + 5 ))
    # A different length per episode, as a real recap has: everything after it
    # then sits at a different offset, which is what lets the search tell the
    # shared card apart from the shared opening behind it.
    recap=$((RECAP_BASE + i * 5))
    recap_audio=$(melody $((200 + i * 60)) $((2 + i % 3)) "$step")
    clip "$work/recap.mkv" "testsrc=s=320x180:r=10" "$recap_audio" "$recap"
    # The black frame that ends the recap carries the episode's own audio. Give
    # it silence and the silence matches across episodes, so the intro search
    # swallows it and the boundary moves a second early.
    clip "$work/black.mkv" "color=c=black:s=320x180:r=10" "$recap_audio" "$BLACK"

    printf "file '%s'\n" "$work/sting.mkv" "$work/recap.mkv" "$work/black.mkv" \
        "$work/intro.mkv" "$work/body.mkv" "$work/credits.mkv" > "$work/list.txt"
    name=$(printf 'Synthetic S01E%02d.mkv' "$i")
    $FFMPEG -y -v error -f concat -safe 0 -i "$work/list.txt" -c copy "$DEST/$name"

    recap_end=$((STING + recap + BLACK))
    intro_end=$((recap_end + INTRO))
    labels+=$(printf '\n  "%s": {"recap": [0, %d], "intro": [%d, %d], "credits": [%d, %d]},' \
        "$name" "$recap_end" "$recap_end" "$intro_end" \
        "$((intro_end + BODY))" "$((intro_end + BODY + CREDITS))")
done

printf '%s\n}\n' "${labels%,}" > "$DEST/labels.json"
echo "wrote $COUNT episodes and labels.json to $DEST"
