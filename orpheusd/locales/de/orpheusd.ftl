# Names Orpheus shows in the audio system's mixers.

# A sink to another node while that node can't play the stream.
sink-unavailable = { $name } (nicht verfügbar)
loopback = Orpheus-Schleife
loopback-playback = { $name } (Wiedergabe)
meter = Orpheus-Pegelmesser { $output }
wire = Orpheus-Verbindung { $from } → { $to }

## Warnungen, die `orpheusctl status` und das Status-JSON zeigen.

ptp-not-started = nicht gestartet
warning-ptp-holdover = PTP angehalten ({ $why }): die Uhr behält Zeit und Gang des Grandmasters bei und läuft langsam von den anderen Knoten weg; PTP startet von selbst wieder
warning-ptp-down = PTP läuft nicht ({ $why }): der Knoten läuft auf der Systemuhr und spielt nicht im Takt mit den anderen Knoten; PTP startet von selbst, sobald es kann
warning-ptp-wifi = PTP läuft über WLAN ({ $interface }): die Uhr pendelt sich womöglich nicht ein, und die Räume laufen auseinander
warning-wifi-grandmaster = { $role ->
        [master] dieser Knoten ist der PTP-Grandmaster über WLAN ({ $interface }): Knoten, die seiner Uhr folgen, pendeln sich womöglich nicht ein; gib einem Knoten am Kabel eine niedrigere ptp.priority1
       *[other] dieser Knoten kann PTP-Grandmaster über WLAN werden ({ $interface }): Knoten, die seiner Uhr folgen, pendeln sich womöglich nicht ein; gib einem Knoten am Kabel eine niedrigere ptp.priority1
    }
warning-ptp-delay = der Weg zum PTP-Grandmaster dauert { $ms } ms, am Kabel deutlich unter { $limit } ms: { $cause }
warning-ptp-steps = die PTP-Uhr ist in der letzten Minute { $count }-mal gesprungen, statt sich einzupendeln: der Weg zum Grandmaster ist langsam oder unstet; { $cause }

cause-unknown = WLAN oder eine überlastete Verbindung auf dem Weg
cause-wifi = dieser Knoten ist im WLAN ({ $interface })
cause-wifi-link = dieser Knoten ist im WLAN ({ $interface }, { $link })
cause-wifi-weak = das WLAN dieses Knotens ist schwach ({ $interface }, { $link }); stell ihn oder den Access Point näher zusammen
wifi-link = Signal { $dbm } dBm, { $mbit } Mbit/s
wifi-link-signal = Signal { $dbm } dBm
wifi-link-rate = { $mbit } Mbit/s

the-sender = der Sender
warning-receiver-multicast-wifi = Empfänger { $receiver } tritt der Multicast-Gruppe { $group } über WLAN bei ({ $interface }), die der Access Point mit seiner niedrigsten Rate an alle Geräte sendet; nimm ein Kabel oder Unicast
warning-receiver-wifi = Empfänger { $receiver } bekommt { $from } über WLAN ({ $interface }); rechne mit Aussetzern, außer link_offset_ms wird erhöht
warning-receiver-peer-wifi = { $peer } sendet aus dem WLAN an Empfänger { $receiver }
warning-jitter = Pakete von { $from } kommen bis zu { $ms } ms gestreut an, am Kabel deutlich unter { $limit } ms: { $cause }; erhöhe link_offset_ms, wenn `late` steigt
warning-held = Empfänger { $receiver } spielt nirgends: der Standardausgang ist eine Senke zu einem anderen Knoten, und Streams werden nur auf Anweisung weitergereicht; wähle einen Ausgang mit `orpheusctl play { $receiver } <Ausgang>` oder setze [receiver] default_output
warning-clipping = { $output } hat Vollaussteuerung erreicht: was darauf spielt, ergibt zusammen mehr, als er wiedergeben kann, und übersteuert; dreh seine Verbindungen leiser (`orpheusctl wire-gain`)

warning-peer-busy = Route { $route }: die Gegenstelle hat keinen freien Empfänger; sie wartet auf einen
warning-peer-incompatible = Route { $route }: keiner der Empfänger der Gegenstelle kann den Stream spielen
warning-sender-multicast-wifi = Route { $route } sendet über WLAN ({ $interface }) an die Multicast-Gruppe { $group }, die der Access Point mit seiner niedrigsten Rate an alle Geräte sendet; nimm ein Kabel oder Unicast
warning-sender-wifi = Route { $route } geht über WLAN hinaus ({ $interface }); rechne mit Aussetzern
warning-sender-peer-wifi = Route { $route }: { $peer } ist im WLAN; rechne mit Aussetzern
warning-peer-jitter = Route { $route }: die Gegenstelle meldet die Pakete über mehr als { $limit } ms gestreut: WLAN oder eine überlastete Verbindung auf dem Weg

-power-save-holds = der Access Point hält die Pakete dieses Knotens zurück, bis er aufwacht, 100 ms und mehr, sodass Uhr und Streams stocken
-power-save-for-good = schalte es dauerhaft aus: NetworkManager `wifi.powersave = 2`, TLP `WIFI_PWR_ON_BAT=off`; siehe die Fehlersuche in der Dokumentation
warning-power-save-no-capability = WLAN-Energiesparen ist an für { $interface }: { -power-save-holds }; der Knoten schaltet es selbst aus, wenn er darf (`sudo setcap cap_net_bind_service,cap_net_admin=ep { $exe }`), oder { -power-save-for-good }
warning-power-save-unsupported = WLAN-Energiesparen ist an für { $interface }, und sein Treiber kann es nicht ausschalten: { -power-save-holds }; nimm ein Kabel oder einen anderen WLAN-Adapter
warning-power-save-failed = WLAN-Energiesparen ist an für { $interface }, und es auszuschalten schlug fehl ({ $error }): { -power-save-holds }; { -power-save-for-good }
warning-power-save-turned-on-again = etwas schaltet WLAN-Energiesparen für { $interface } immer wieder ein (NetworkManager beim Verbinden, TLP im Akkubetrieb); der Knoten schaltet es binnen 30 s wieder aus, aber bis dahin gilt: { -power-save-holds }; { -power-save-for-good }

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
