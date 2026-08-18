-- The memory that a track was OCRed and yielded NOTHING. Without it the
-- idle sweep read "no text" as a failure and asked again on every hub
-- start: display sets re-fetched, ~15 s of Tesseract, the same empty
-- answer. Same bytes, same model, same result - a fact, not weather.
--
-- Keyed to the parent track row, CASCADE so replacing or deleting the
-- source re-asks naturally. `model` records which traineddata gave the
-- empty answer; a newly installed, better model is re-asked for by
-- clearing the row (there is no automatic re-ask on model change).
CREATE TABLE ocr_no_text (
    track_id INTEGER PRIMARY KEY REFERENCES subtitle_tracks(id) ON DELETE CASCADE,
    model    TEXT NOT NULL,
    at       INTEGER NOT NULL
);
