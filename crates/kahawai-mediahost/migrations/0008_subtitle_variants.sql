UPDATE catalog_files SET reprobe_required=1
 WHERE error='' AND json_array_length(streams_json,'$.subtitles') > 0;
