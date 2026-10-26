# Was orpheusctl zeigt; die Erklärungen stehen in der englischen Datei.

## Hilfe.

about = Kommandozeilen-Client für Orpheus-Knoten
help-usage = Aufruf:
help-commands = Befehle
help-arguments = Argumente
help-options = Optionen
help-help = Hilfe zeigen
help-version = Version zeigen
cmd-help = Diese Hilfe oder die Hilfe zum angegebenen Befehl zeigen
arg-help-command = Der Befehl, dessen Hilfe gezeigt wird

arg-node = Der Knoten, mit dem gesprochen wird: ein Hostname oder eine Adresse (API auf Port 7480) oder die URL seiner API; localhost, wenn weder dies noch ORPHEUS_NODE gesetzt ist.
arg-json = Die JSON-Antworten des Knotens ausgeben statt Text.
cmd-status = ID, Name, Version und Streams des Knotens zeigen.
arg-status-watch = Dem Knoten folgen: neu zeigen, wenn er sich ändert, höchstens alle SECONDS Sekunden (Standard 1), Zähler als Raten.
cmd-connect = Den Ton des Knotens an eine Gegenstelle senden: legt eine Senke "<name>" an, deren Ton an einen freien Empfänger der Gegenstelle gestreamt wird, und verbindet diesen Empfänger damit. Eine frühere Route zur Gegenstelle mit derselben Priorität kommt so zurück, wie sie war.
arg-connect-peer = Gegenstelle: ihr Name oder ihre ID (siehe `nodes`), Hostname oder Adresse, oder die URL ihrer API. Mit --send-only: Hostname oder Adresse, wahlweise mit :Port (Standard 5004); IPv6 mit Port als [Adresse]:Port.
arg-connect-name = Name, den die Senke zeigt; Standard ist der frühere Name der Route, sonst der Name der Gegenstelle.
arg-connect-send_only = Nur senden; keinen Empfänger der Gegenstelle verbinden (für Empfänger ohne NMOS-API oder solche, die jeden Stream annehmen).
arg-connect-force = Den Empfänger der Gegenstelle übernehmen, auch wenn ein anderer Sender mit ihm verbunden ist.
arg-connect-priority = `normal` (Standard) oder `high`: spielt über die anderen Streams der Gegenstelle und macht sie solange leiser, gleich welche Regel sie hat (für Durchsagen).
arg-connect-layout = Was die Kanäle der Senke tragen und damit die des Streams: ST-2110-30-Gruppen (`51`, `71`, `U16`) oder Kanalsymbole (`"L R C"`); Standard ist das frühere Layout der Route, sonst Stereo.
cmd-nodes = Die anderen Knoten im Netz auflisten.
cmd-alias = Einem anderen Knoten einen eigenen Namen geben, der hier statt des angekündigten erscheint: in `nodes`, in Senken zu ihm und in Streams von ihm.
arg-alias-node = Der Knoten: sein Name, Alias oder seine ID (siehe `nodes`).
arg-alias-alias = Der neue Alias.
arg-alias-remove = Den Alias entfernen und wieder den angekündigten Namen zeigen.
cmd-pin = Einen anderen Knoten gelistet lassen, solange er offline ist, auch nach einem Neustart.
arg-pin-node = Der Knoten: sein Name, Alias oder seine ID (siehe `nodes`).
cmd-unpin = Einen Knoten nicht mehr gelistet lassen, solange er offline ist.
arg-unpin-node = Der Knoten: sein Name, Alias oder seine ID (siehe `nodes`).
cmd-outputs = Die Ausgänge des Knotens auflisten, ihre Typen und Kanäle.
cmd-inputs = Die Eingänge des Knotens auflisten: die Streams der Empfänger und die Eingänge des Audiosystems, etwa Mikrofone.
cmd-output = Ausgänge aus Kanälen eines anderen anlegen oder entfernen.
cmd-output-create = Einen Ausgang anlegen, der auf gewählten Kanälen eines anderen spielt, etwa auf den Kanälen 3 und 4 eines Audio-Interfaces. Anwendungen und Empfänger spielen hinein wie in jeden Ausgang; bleibt über Neustarts erhalten.
arg-output-create-name = Sein Name, z. B. Küche.
arg-output-create-device = Der Ausgang, auf dem er spielt: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat.
arg-output-create-channels = Die Kanäle dieses Ausgangs, ab 1 gezählt, einer pro Kanal des neuen Ausgangs: `3 4`.
arg-output-create-layout = Was die Kanäle tragen, als ST-2110-30-Gruppen oder Symbole; Standard ist Stereo für zwei und so weiter.
cmd-output-remove = Einen angelegten Ausgang entfernen.
arg-output-remove-output = Der Ausgang: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat.
cmd-set-type = Festlegen, welche Art Gerät ein Ausgang ist, was andere Knoten zeigen.
arg-set-type-output = Der Ausgang: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat.
arg-set-type-type = speaker, headphones, headset, tv, hifi, computer, portable, phone oder generic; `auto` geht zum erkannten Typ zurück.
cmd-play = Den Stream eines Empfängers auf einem anderen Ausgang spielen, statt auf den Verbindungen seiner Quelle; auch einen geparkten. Der Knoten merkt es sich für das, was gesendet wird, und spielt es später wieder dort.
arg-play-receiver = Empfänger-ID, wie `status` sie zeigt.
arg-play-output = Der Ausgang: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat; `default` geht zum Standardausgang zurück.
cmd-default = Einen Ausgang zum Standardausgang machen: Anwendungen spielen dort, und ebenso empfangene Streams ohne eigenen Ausgang. Die Senke eines anderen Knotens geht auch. Für Knoten ohne Desktop.
arg-default-output = Der Ausgang: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat.
cmd-settings = Die Einstellungen zeigen, die sich im Betrieb ändern lassen, und woher jede kommt: Standard, config.toml oder `set`.
cmd-set = Einstellungen ändern, über Neustarts hinweg (config.toml bleibt unberührt).
arg-set-settings = ABSCHNITT.SCHLÜSSEL=WERT, z. B. receiver.policy=mix oder ptp.domain=1.
cmd-reset = `set` rückgängig machen: zurück zu config.toml oder zum Standard.
arg-reset-keys = ABSCHNITT.SCHLÜSSEL, oder ABSCHNITT für alle seine Schlüssel.
cmd-routes = Die laufenden Routen auflisten und die gemerkten von früher.
cmd-disconnect = Eine mit `connect` gestartete Route stoppen und den Empfänger der Gegenstelle trennen. Die Route wird für das nächste `connect` gemerkt.
arg-disconnect-route = Die Route: ihr Name, ihre ID oder deren Anfang (siehe `routes`).
cmd-forget = Eine Route vergessen, sodass das nächste `connect` eine neue startet; trennt sie vorher, wenn sie läuft.
arg-forget-route = Die Route: ihr Name, ihre ID oder deren Anfang (siehe `routes`).
cmd-route-layout = Festlegen, was die Kanäle der Senke einer Route tragen, und damit, wie viele Kanäle ihr Stream hat; die Senke startet neu, und ebenso der Empfänger der Gegenstelle.
arg-route-layout-route = Die Route: ihr Name, ihre ID oder deren Anfang (siehe `routes`).
arg-route-layout-layout = ST-2110-30-Gruppen (`ST`, `51`, `71`, `U16`) oder Kanalsymbole (`L R C`).
cmd-sources = Die Quellen auflisten, die sich die Empfänger merken: wo jede spielt und wie laut.
cmd-layout = Festlegen, was die Kanäle einer Quelle tragen, für Sender, die nichts oder Falsches sagen; ein Stream, der sie spielt, startet neu.
arg-layout-source = Die Quelle: ihr Name, ihre ID oder deren Anfang (siehe `sources`).
arg-layout-layout = ST-2110-30-Gruppen (`51`, `ST,M`, `DM`, `U02`), Kanalsymbole (`L R`, `M1 M2`), oder `auto` für das, was der Stream sagt.
cmd-wire = Kanäle einer Quelle oder eines Eingangs auf einem Ausgang spielen und die Verbindung zwischen beiden ersetzen. Eine Quelle mit Verbindungen spielt nur auf diesen; was mehrere Verbindungen in einen Kanal bringen, wird addiert.
arg-wire-from = Die Quelle oder der Eingang: sein Name, oder die ID einer Quelle oder deren Anfang (siehe `sources` und `inputs`).
arg-wire-output = Der Ausgang: sein Name oder seine Beschreibung, oder ein Teil davon, den nur er hat.
arg-wire-channels = Ausgangskanal = Eingangskanal, jeweils als Nummer (ab 1), Symbol oder Bezeichnung, wahlweise mit einer Verstärkung in dB nach `@`: `L=R R=L`, `1=U03@-6`, `2=1`. Ohne geht jeder Kanal dorthin, wo er hingehört, und die auf andere gefalteten mit -3 dB.
arg-wire-gain = Die Verstärkung der Verbindung in dB, von -90 bis +12; 0 spielt die Kanäle, wie sie sind.
arg-wire-mute = Stummschalten.
cmd-wire-gain = Die Verstärkung einer Verbindung ändern und ihre Kanäle behalten; die Änderung gleitet über 20 ms.
arg-wire-gain-from = Die Quelle oder der Eingang, wie bei `wire`.
arg-wire-gain-output = Der Ausgang, wie bei `wire`.
arg-wire-gain-gain = Eine Verstärkung in dB von -90 bis +12 (`-6`, `0`), `mute` oder `unmute`.
cmd-unwire = Eine Verbindung oder einige ihrer Kanäle entfernen. Eine Quelle ohne Verbindungen wird geparkt, bis sie mit `play` gespielt oder wieder verbunden wird.
arg-unwire-from = Die Quelle oder der Eingang, wie bei `wire`.
arg-unwire-output = Der Ausgang, wie bei `wire`.
arg-unwire-channels = Die zu entfernenden Kanalpaare, wie bei `wire`; ohne alle.
cmd-patch = Die Verbindungen zeigen: was auf jedem Ausgangskanal spielt und wie laut: die Spitze der letzten Sekunde jedes Ausgangskanals und dessen, was jede Verbindung ihm bringt.
arg-patch-watch = Alle SECONDS Sekunden (Standard 1) neu zeigen, als Aussteuerungsanzeige.
cmd-forget-source = Vergessen, wo eine Quelle spielt und wie laut; spielt sie gerade, geht sie mit voller Lautstärke auf den Standardausgang.
arg-forget-source-source = Die Quelle: ihr Name, ihre ID oder deren Anfang (siehe `sources`).

## Fehler.

watch-too-short = --watch braucht mindestens 0,1 Sekunden
expected-setting = { $arg }: erwartet ABSCHNITT.SCHLÜSSEL=WERT
expected-key = { $arg }: erwartet ABSCHNITT.SCHLÜSSEL
not-a-gain = "{ $gain }" ist keine Verstärkung: gib dB, mute oder unmute an
bad-gain-in-pair = "{ $pair }": "{ $db }" ist keine Verstärkung in dB
bad-pair = "{ $pair }" ist nicht AUS=EIN, z. B. L=R, 1=U03 oder L=C@-3
no-wire = keine Verbindung von { $from } zu { $output }
wire-plays-all = die Verbindung spielt alle Kanäle; `orpheusctl unwire "{ $from }" "{ $output }"` entfernt sie
wire-lacks = die Verbindung hat kein { $pair }
pick-none = { $list ->
        [routes] keine Route "{ $query }"; siehe `orpheusctl routes`
       *[sources] keine Quelle "{ $query }"; siehe `orpheusctl sources`
    }
pick-many = "{ $query }" passt zu den { $list ->
        [routes] Routen
       *[sources] Quellen
    } { $ids }; gib die ID an
no-source-or-input = keine Quelle und kein Eingang "{ $from }"; siehe `orpheusctl sources` und `orpheusctl inputs`
from-ambiguous = "{ $from }" passt zu mehr als einer Quelle oder einem Eingang; sei genauer
from-ambiguous-id = "{ $from }" passt zu mehr als einer Quelle oder einem Eingang; gib die ID einer Quelle an
no-node = kein Knoten "{ $query }"; siehe `orpheusctl nodes`
not-orpheus = { $query } ist kein Orpheus-Knoten
no-output = kein Ausgang passt zu "{ $query }"; siehe `orpheusctl outputs`
output-ambiguous = "{ $query }" passt zu { $outputs }; sei genauer
refused = der Knoten lehnt ab: { $message } (HTTP { $status })
not-running = orpheusd läuft nicht auf diesem Rechner
not-running-hint = Starte ihn mit orpheusd, oder wähle einen Knoten mit --node oder ORPHEUS_NODE.
no-node-at = kein Orpheus-Knoten unter { $base }: { $reason }
no-node-at-hint = Läuft orpheusd dort? Wähle einen Knoten mit --node oder ORPHEUS_NODE.
reason-host-not-found = Host nicht gefunden
reason-connection-failed = Verbindung fehlgeschlagen
reason-timed-out = Zeitüberschreitung
reason-connection-refused = Verbindung abgelehnt
reason-connection-reset = Verbindung zurückgesetzt
reason-host-unreachable = Host nicht erreichbar
reason-network-unreachable = Netz nicht erreichbar
reason-address-not-available = Adresse nicht verfügbar
node-closed = der Knoten hat die Verbindung geschlossen
bad-node-url = ungültige Knoten-URL { $base }
watch-needs-http = Verfolgen braucht eine http://-URL des Knotens, nicht { $base }

querying = beim Abfragen von { $url }
connecting-via = beim Verbinden mit { $peer } über { $url }
connecting = beim Verbinden mit { $base }
looking-up = beim Auflösen von { $host }
opening = beim Öffnen von { $url }
reading-reply = beim Lesen der Antwort von { $url }
reading = beim Lesen { $what ->
        [nodes] der Knoten
        [outputs] der Ausgänge
        [inputs] der Eingänge
        [routes] der Routen
        [sources] der Quellen
        [wires] der Verbindungen
        [wire] der Verbindung
        [levels] der Pegel
        [settings] der Einstellungen
        [status] des Status
        [event] eines Ereignisses
       *[events] der Ereignisse
    }
reading-from = beim Lesen { $what ->
        [nodes] der Knoten
        [routes] der Routen
        [sources] der Quellen
       *[status] des Status
    } von { $url }
creating-via = beim Anlegen von { $name } über { $url }
removing-via = beim Entfernen von { $what } über { $url }
resetting-via = beim Zurücksetzen über { $url }
setting-via = beim Setzen von { $what } über { $url }
moving-via = beim Verschieben über { $url }
setting-default-via = beim Setzen des Standards über { $url }
disconnecting-via = beim Trennen von { $route } über { $url }
forgetting-via = beim Vergessen von { $what } über { $url }
setting-layout-via = beim Setzen des Layouts von { $what } über { $url }
wiring-via = beim Verbinden von { $from } mit { $output } über { $url }
setting-gain-via = beim Setzen der Verstärkung von { $from } → { $output } über { $url }
unwiring = beim Trennen von { $from } und { $output }
unwiring-channels-via = beim Trennen von Kanälen über { $url }
changing-node-via = beim Ändern von Knoten { $id } über { $url }
changing-settings-via = beim Ändern der Einstellungen über { $url }

## Ergebnisse.

route-created = Route { $id } "{ $name }" → { $to } ({ $receiver ->
        [yes] Empfänger verbunden
       *[no] nur Senden
    })
output-created = Ausgang { $name }

## Status.

heading-audio = Audio
heading-warnings = Warnungen
heading-clock = Uhr
heading-loopback = Schleife
heading-routes = Routen
receiver-title = Empfänger { $id }
no-audio = kein Audiosystem, keine Streams
system-clock = Systemuhr
ptp-holding-over = PTP gestoppt, hält die Uhr
ptp-not-running = PTP läuft nicht, Systemuhr
ptp-settled = PTP, eingeschwungen
ptp-settling = PTP, schwingt ein
ptp-state = { $state ->
        [listening] lauscht
        [master] Master
        [slave] Slave
        [passive] passiv
        [uncalibrated] unkalibriert
        [pre_master] wird Master
        [faulty] fehlerhaft
        [disabled] aus
       *[other] { $state }
    }
ptp-heading-state = PTP, { $state }
this-node = dieser Knoten
key-error = Fehler
key-interface = Schnittstelle
key-identity = Identität
key-state = Zustand
key-grandmaster = Grandmaster
key-offset = Abweichung
key-delay = Laufzeit
key-frequency = Frequenz
key-steps = Sprünge
key-listen = Lauscht
key-channels = Kanäle
key-output = Ausgang
key-mix = Mischung
key-priority = Priorität
key-link-offset = Vorlauf
key-level = Pegel
key-margin = Reserve
key-jitter = Jitter
key-packets = Pakete
key-ignored = Ignoriert
key-buffer = Puffer
key-asrc = ASRC
key-problems = Probleme
frames = { $n } Frames
connected-off = verbunden mit { $from }, aus
free-port = frei, Port { $port }
receiving-from = empfängt von { $from }
receiving-muted = empfängt von { $from }, stumm
receiving-ducked = empfängt von { $from }, abgesenkt
receiving-ssrc = empfängt SSRC { $ssrc }
connected-idle = verbunden mit { $from }, ruht
free-any = frei, spielt jeden Sender
idle = ruht
parked = geparkt
parked-lost = geparkt: { $output } ist weg
parked-unwired = geparkt: seine letzte Verbindung wurde entfernt
held = keiner: der Standardausgang ist die Senke eines anderen Knotens
on-wires = auf Verbindungen, siehe `orpheusctl patch`
priority-high = hoch
silence = Stille
silent = still
none = keine
count-lost = { $n } verloren
count-late = { $n } zu spät
count-early = { $n } zu früh
count-malformed = { $n } fehlerhaft
count-realign = { $count ->
        [one] { $n } Neuausrichtung
       *[other] { $n } Neuausrichtungen
    }
count-underrun = { $count ->
        [one] { $n } Unterlauf
       *[other] { $n } Unterläufe
    }
count-overrun = { $count ->
        [one] { $n } Überlauf
       *[other] { $n } Überläufe
    }
count-resync = { $count ->
        [one] { $n } Neusynchronisation
       *[other] { $n } Neusynchronisationen
    }
count-send-error = { $count ->
        [one] { $n } Sendefehler
       *[other] { $n } Sendefehler
    }
peer-disconnected = getrennt, pausiert
peer-busy = wartet auf einen freien Empfänger
peer-incompatible = Gegenstelle kann diesen Stream nicht spielen
peer-offline = Gegenstelle offline
peer-unreachable = nicht erreichbar
peer-connecting = verbindet
peer-idle = ruht
peer-off = aus
peer-output-gone = ihr Ausgang dort ist weg
peer-unwired = dort ohne Verbindung
peer-sink-hidden = Senke verborgen
peer-muted = durch Regel stumm
peer-ducked = abgesenkt
peer-high-priority = hohe Priorität

## Tabellen.

col-id = ID
col-name = NAME
col-peer = GEGENSTELLE
col-packets = PAKETE
col-buffer = PUFFER
col-asrc = ASRC
col-problems = PROBLEME
col-output = AUSGANG
col-type = TYP
col-channels = KANÄLE
col-input = EINGANG
col-kind = ART
col-volume = LAUTSTÄRKE
col-layout = LAYOUT
col-state = ZUSTAND
col-priority = PRIORITÄT
col-setting = EINSTELLUNG
col-value = WERT
col-from = HERKUNFT
col-address = ADRESSE
col-version = VERSION
col-link = VERBINDUNG
no-outputs = keine Ausgänge
no-inputs = keine Eingänge
no-sources = keine Quellen
no-routes = keine Routen
no-nodes = keine anderen Knoten gefunden
type-set = { $type } (gesetzt)
created-on = auf { $device } { $channels }
created-on-missing = auf { $device } { $channels } (fehlt)
plays-receivers = Empfänger { $ids }
default = Standard
input-kind = { $kind ->
        [receiver] Empfänger
        [device] Gerät
       *[other] { $kind }
    }
setting-from = { $from ->
        [default] Standard
        [config] config.toml
        [api] set
       *[other] { $from }
    }
playing-on-receiver = spielt auf Empfänger { $receiver }
ago = { $days ->
        [0] heute
        [1] gestern
       *[other] vor { $days } Tagen
    }
last-played = zuletzt gespielt { $ago }
as-sent = wie gesendet
remembered = gemerkt
remembered-last-used = gemerkt, zuletzt genutzt { $ago }
route-connected = verbunden
route-idle = ruht, belegt einen Empfänger, sobald Ton spielt
route-busy = wartet auf einen freien Empfänger
route-incompatible = Gegenstelle kann diesen Stream nicht spielen
route-offline = Gegenstelle offline
route-unreachable = Gegenstelle nicht erreichbar
route-disconnected = getrennt, pausiert
online = online
online-pinned = online, angeheftet
offline = offline
offline-pinned = offline, angeheftet
wired = Kabel
wifi = WLAN

## Verbindungen.

muted = stumm
gain-muted = { $gain }, stumm
mix = { $mix ->
        [downmix] Downmix
        [upmix] Upmix
        [remix] Remix
       *[other] { $mix }
    }
mix-layouts = { $mix } { $from } → { $to }
mix-left-out = { $mix }, ohne { $channels }
plays-nowhere = { $name } spielt nirgends
is-missing = { $name } fehlt
no-channels-match = keine Kanäle passen
waiting-because = wartet, { $why }
no-wires = keine Verbindungen
heading-waiting = Wartend
all-channels = alle
clipped-ago = übersteuert vor { $secs } s
linking = { $wire } (verbindet)
