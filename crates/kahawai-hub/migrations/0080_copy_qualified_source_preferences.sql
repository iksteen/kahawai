INSERT INTO user_prefs(user_id,scope,key,value)
SELECT p.user_id,'source:' || ps.item_id || ':' || ps.id,
       CASE WHEN p.key='audio' THEN 'audio.track' ELSE p.key END,p.value
FROM user_prefs p JOIN playable_sources ps ON ps.item_id=p.scope
WHERE (p.key IN('audio.track','subs.track') OR (p.key='audio' AND p.value LIKE '#%'))
  AND (SELECT COUNT(*) FROM playable_sources WHERE item_id=p.scope)=1
ON CONFLICT DO NOTHING;
