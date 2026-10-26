# Names Orpheus shows in the audio system's mixers.

# A sink to another node while that node can't play the stream.
sink-unavailable = { $name } (nicht verfügbar)
loopback = Orpheus-Schleife
loopback-playback = { $name } (Wiedergabe)
meter = Orpheus-Pegelmesser { $output }
wire = Orpheus-Verbindung { $from } → { $to }

## Fehler in Einstellungen.

setting-range = { $key } muss zwischen { $min } und { $max } liegen
setting-empty = { $key } darf nicht leer sein
setting-above-zero = { $key } muss größer als 0 sein
setting-thresholds = [ptp] step_threshold_us und settle_threshold_us müssen größer als 0 sein
setting-language = [ui] language "{ $language }" ist weder auto noch ein Sprachkürzel wie de
setting-ports = [receiver] port + 2 × (max − 1) muss unter 65536 bleiben
setting-block-both = setze entweder block_frames oder cycle_ms, nicht beides
setting-block = der Block muss { $min } bis { $max } Frames lang sein (bei 48 kHz { $min_ms } bis { $max_ms } ms), nicht { $frames }
settings-not-object = die Einstellungen müssen ein JSON-Objekt sein
settings-section-not-object = { $section } muss ein Objekt sein
settings-unknown-section = unbekannter Abschnitt { $section }
settings-invalid = ungültige Einstellungen: { $error }
