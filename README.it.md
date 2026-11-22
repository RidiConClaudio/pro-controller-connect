# Pro Controller Connect

[English](README.md) · **Italiano**

App per **Windows, Linux e macOS** che riconnette automaticamente il Pro Controller al Bluetooth del computer,
senza dover rimuovere a mano il dispositivo dalle impostazioni ogni volta.

<p align="center">
  <img src="docs/screenshot.png" width="380" alt="Finestra principale con test dei tasti">
  <img src="docs/settings.png" width="380" alt="Impostazioni">
</p>

## Il problema

Dopo un periodo di inattività o un riavvio del PC, Windows mantiene un abbinamento
"bloccato" con il controller: il dispositivo risulta abbinato ma non si connette più, e
finché è in lista Windows non lo vede nemmeno quando premi SYNC. Di solito bisogna
rimuoverlo dalle impostazioni Bluetooth, riavviare la ricerca e premere di nuovo SYNC.

## Cosa fa

1. **Rimuove** il vecchio abbinamento
2. **Cerca** il controller in modalità SYNC
3. **Abbina** di nuovo, senza PIN né conferme
4. **Verifica** la connessione, anche con un **test dei tasti in tempo reale**

Inoltre:

- **Modalità automatica**: se il controller resta abbinato ma scollegato per 15 secondi,
  l'app rimuove da sola l'abbinamento e resta in ascolto. Quando vuoi giocare ti basta premere SYNC.
- **Test in tempo reale**: un controller disegnato in cui ogni tasto premuto si illumina, con
  chip di mappatura, valori degli stick e ora dell'ultimo input.
- **6 lingue**: English, Italiano, Español, Français, Deutsch, Português. "Auto" segue la lingua di Windows.
- **Avvio con Windows**, avvio silenzioso nell'area di notifica, notifiche.
- **Riavvia Bluetooth**: spegne e riaccende la radio, utile se l'adattatore si blocca.
- Registro delle attività.

## Download

Scarica la versione per il tuo sistema dalla pagina [Releases](../../releases), oppure compilala (vedi sotto):

- **Windows**: `ProControllerConnect.exe` (Windows 10 versione 2004 o Windows 11, singolo file portatile da ~1 MB con Edge WebView2).
- **Linux**: `ProControllerConnect-linux-x86_64.tar.gz` (usa BlueZ `bluetoothctl`, GTK3 + WebKit2GTK 4.1 e `/dev/input/js*`).
- **macOS**: `ProControllerConnect-macos-universal.zip` (`Pro Controller Connect.app`, binario universale per Apple Silicon e Intel, usa `IOBluetooth` / `blueutil` e WKWebView).

Gli eseguibili non sono firmati digitalmente: su Windows SmartScreen scegli *Ulteriori informazioni › Esegui comunque*; su macOS fai clic destro su `Pro Controller Connect.app` › *Apri* (oppure `xattr -cr "Pro Controller Connect.app"`).

## Uso

- **Connetti controller** avvia la procedura. Poi tieni premuto per un secondo il tasto SYNC
  (il piccolo tasto rotondo sul bordo superiore, accanto alla porta USB-C) finché i LED scorrono.
- Chiudere la finestra la nasconde soltanto. Per uscire: clic destro sull'icona nell'area di notifica › **Esci**.
- Il test dei tasti funziona quando la finestra è in primo piano (limite di Windows).

Impostazioni e log sono in:
- **Windows**: `%LOCALAPPDATA%\you_are_incredible\ProControllerConnect\`
- **Linux**: `~/.config/you_are_incredible/ProControllerConnect/`
- **macOS**: `~/Library/Application Support/you_are_incredible/ProControllerConnect/`

In `settings.json` puoi cambiare:

| Chiave | Default | Significato |
| --- | --- | --- |
| `Language` | `auto` | `auto`, `en`, `it`, `es`, `fr`, `de`, `pt` |
| `NameFilter` | `Pro Controller` | Nomi Bluetooth da gestire, separati da `;` (es. `Pro Controller;Joy-Con`) |
| `GraceSeconds` | `15` | Attesa prima della riparazione automatica |

## Compilazione

Serve [Rust](https://rustup.rs/).
- **Windows**: toolchain MSVC.
- **Linux** (richiede `webkit2gtk-4.1` e `bluez-utils` / `bluetoothctl`):
  - **CachyOS / Arch Linux**: `sudo pacman -S webkit2gtk-4.1 gtk3 bluez-utils`
  - **Ubuntu / Debian / Mint**: `sudo apt install libgtk-3-dev libwebkit2gtk-4.1-dev bluez build-essential pkg-config`
  - **Fedora / Bazzite**: `sudo dnf install gtk3-devel webkit2gtk4.1-devel bluez`
- **macOS**: Xcode Command Line Tools (`xcode-select --install`).

```bash
cargo build --release
```

Il risultato è `target/release/ProControllerConnect` (`.exe` su Windows). L'icona Windows si rigenera con `tools/make-icon.ps1`.

## Traduzioni

Tutti i testi sono in [`src/loc.json`](src/loc.json), condiviso dall'app e dalla sua finestra. Per aggiungere
una lingua copia la tabella `en`, traducila e aggiungi il codice in `LANGUAGES` in `src/loc.rs`.
Le pull request sono benvenute.

Progetto non affiliato né approvato da Nintendo. "Pro Controller" è citato solo per indicare
il dispositivo compatibile.

## Licenza

[MIT](LICENSE)
