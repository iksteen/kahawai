WITH projected_copies AS (
    SELECT a.library_item_id,i.id AS collection_item_id,p.library_item_id AS series_id,
           i.season,i.episode
    FROM collection_items i
    JOIN collection_item_library_items a ON a.collection_item_id=i.id AND a.ordinal=1
    JOIN episode_details e ON e.item_id=a.library_item_id
    JOIN collection_item_library_items p ON p.collection_item_id=i.parent_id AND p.ordinal=1
    LEFT JOIN item_match own ON own.item_id=i.id
    LEFT JOIN item_match parent ON parent.item_id=i.parent_id
    JOIN provider_metadata pm ON pm.item_id=i.id AND pm.provider=COALESCE(own.provider,parent.provider)
      AND pm.provider_id<>'' AND (own.provider_id IS NULL OR pm.provider_id=own.provider_id)
    WHERE i.kind='episode' AND i.assignment_manual=0 AND i.episode IS NOT NULL
      AND COALESCE(i.episode_end,i.episode)=i.episode
      AND (SELECT COUNT(*) FROM collection_item_library_items all_links WHERE all_links.collection_item_id=i.id)=1
      AND NOT EXISTS(SELECT 1 FROM manual_match pin WHERE pin.item_id=i.id)
      AND NOT EXISTS(SELECT 1 FROM rejected_matches rj WHERE rj.item_id=pm.item_id AND rj.provider=pm.provider AND rj.provider_id=pm.provider_id)
      AND e.series_id=p.library_item_id
      AND e.numbering=CASE WHEN COALESCE(pm.proj_season,i.season) IS NULL THEN 'absolute' ELSE 'aired' END
      AND e.season IS COALESCE(pm.proj_season,i.season)
      AND e.episode=COALESCE(pm.proj_episode,i.episode)
      AND (e.season IS NOT i.season OR e.episode IS NOT i.episode
        OR e.numbering<>CASE WHEN i.season IS NULL THEN 'absolute' ELSE 'aired' END)
), recoverable_items AS (
    SELECT library_item_id FROM projected_copies
    GROUP BY library_item_id
    HAVING COUNT(DISTINCT json_array(series_id,season,episode))=1
      AND COUNT(*)=(SELECT COUNT(*) FROM collection_item_library_items all_links
                    WHERE all_links.library_item_id=projected_copies.library_item_id)
)
UPDATE library_items SET unidentified=1
WHERE kind='episode' AND merged_into IS NULL
  AND id IN(SELECT library_item_id FROM recoverable_items);

INSERT INTO library_pending(collection_item_id)
SELECT DISTINCT i.id FROM collection_items i
JOIN collection_item_library_items a ON a.collection_item_id=i.id
JOIN episode_details e ON e.item_id=a.library_item_id
WHERE i.kind='episode' AND i.assignment_manual=0 AND i.episode IS NOT NULL
  AND COALESCE(i.episode_end,i.episode)=i.episode
  AND NOT EXISTS(SELECT 1 FROM manual_match pin WHERE pin.item_id=i.id)
  AND (e.season IS NOT i.season OR e.episode IS NOT i.episode
    OR e.numbering<>CASE WHEN i.season IS NULL THEN 'absolute' ELSE 'aired' END)
ON CONFLICT DO NOTHING;
