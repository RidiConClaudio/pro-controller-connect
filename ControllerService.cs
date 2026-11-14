using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Windows.Devices.Bluetooth;
using Windows.Devices.Enumeration;
using Windows.Devices.Radios;
using Windows.Foundation;

namespace ProControllerConnect;

public enum LinkState { Starting, Connected, Disconnected, NotPaired, Removing, Searching, Pairing, BluetoothOff, NoAdapter, Error }

/// <summary>Passi della procedura di riabbinamento (usati dallo stepper della UI).</summary>
public enum FlowStep { None = 0, Remove = 1, Sync = 2, Pair = 3, Done = 4 }

public sealed record ServiceStatus(LinkState State, FlowStep Step, string Title, string Detail, bool Busy, bool AutoFlow);

public sealed record ControllerInfo(DeviceInformation Info, string Name, bool Connected);

/// <summary>
/// Gestisce l'intero ciclo: monitoraggio del controller abbinato, rimozione dell'abbinamento
/// "bloccato", ricerca del controller in modalità SYNC e nuovo abbinamento automatico.
/// </summary>
public sealed class ControllerService : IDisposable
{
    static readonly string[] AepProps =
    {
        "System.Devices.Aep.DeviceAddress",
        "System.Devices.Aep.IsConnected",
        "System.Devices.Aep.IsPaired",
    };

    readonly AppSettings _settings;
    readonly SemaphoreSlim _flowLock = new(1, 1);
    readonly CancellationTokenSource _loopCts = new();
    CancellationTokenSource? _flowCts;
    bool _flowIsAuto;
    bool _autoSuspended;
    bool _wasConnected;
    DateTime? _disconnectedSince;
    DateTime _holdUntil = DateTime.MinValue;

    public event Action<ServiceStatus>? StatusChanged;
    public event Action<string>? Log;
    public event Action<string, string>? Notify;

    public ServiceStatus Current { get; private set; } =
        new(LinkState.Starting, FlowStep.None, "Avvio…", "Controllo del Bluetooth in corso", true, false);

    public bool FlowRunning => _flowCts != null;

    public ControllerService(AppSettings settings) => _settings = settings;

    public void Start() => _ = Task.Run(() => MonitorLoopAsync(_loopCts.Token));

    // ───────────────────────────── API pubblica ─────────────────────────────

    /// <summary>Procedura manuale: rimuove, cerca (90 s) e riabbina.</summary>
    public Task ConnectNowAsync()
    {
        _autoSuspended = false;
        _holdUntil = DateTime.MinValue;
        return RunFlowAsync(isAuto: false);
    }

    public void CancelFlow()
    {
        if (_flowCts == null) return;
        if (_flowIsAuto) _autoSuspended = true; // l'utente ha interrotto: niente ripartenza immediata
        _flowCts.Cancel();
    }

    public void OnAutoModeChanged()
    {
        _autoSuspended = false;
        _holdUntil = DateTime.MinValue;
        if (!_settings.AutoMode && _flowIsAuto) _flowCts?.Cancel();
    }

    public async Task RestartBluetoothAsync()
    {
        CancelFlow();
        try
        {
            var access = await Radio.RequestAccessAsync();
            if (access != RadioAccessStatus.Allowed)
            {
                WriteLog($"Accesso alla radio negato ({access}). Controlla Impostazioni › Privacy › Radio.");
                return;
            }
            var radio = await GetRadioAsync();
            if (radio == null) { WriteLog("Nessuna radio Bluetooth trovata."); return; }

            WriteLog("Riavvio della radio Bluetooth…");
            Publish(LinkState.Removing, FlowStep.None, "Riavvio del Bluetooth…", "Spegnimento e riaccensione della radio", force: true);
            _holdUntil = DateTime.UtcNow.AddSeconds(5);
            await radio.SetStateAsync(RadioState.Off);
            await Task.Delay(2500);
            await radio.SetStateAsync(RadioState.On);
            WriteLog("Radio Bluetooth riaccesa.");
            _holdUntil = DateTime.UtcNow.AddSeconds(2);
        }
        catch (Exception ex)
        {
            WriteLog("Riavvio Bluetooth fallito: " + ex.Message);
        }
    }

    // ───────────────────────────── Monitoraggio ─────────────────────────────

    async Task MonitorLoopAsync(CancellationToken ct)
    {
        WriteLog("Servizio avviato. Filtro dispositivi: " + _settings.NameFilter);
        while (!ct.IsCancellationRequested)
        {
            try
            {
                if (!FlowRunning && DateTime.UtcNow >= _holdUntil) await TickAsync();
            }
            catch (Exception ex)
            {
                WriteLog("Errore di monitoraggio: " + ex.Message);
            }
            try { await Task.Delay(2000, ct); } catch (OperationCanceledException) { break; }
        }
    }

    async Task TickAsync()
    {
        var adapter = await BluetoothAdapter.GetDefaultAsync();
        if (adapter == null)
        {
            Publish(LinkState.NoAdapter, FlowStep.None, "Bluetooth non disponibile",
                "Nessun adattatore Bluetooth trovato su questo PC");
            return;
        }
        var radio = await adapter.GetRadioAsync();
        if (radio != null && radio.State != RadioState.On)
        {
            _disconnectedSince = null;
            Publish(LinkState.BluetoothOff, FlowStep.None, "Bluetooth disattivato",
                "Attiva il Bluetooth o usa «Riavvia Bluetooth»");
            return;
        }

        var paired = await GetPairedControllersAsync();
        var connected = paired.FirstOrDefault(c => c.Connected);
        bool auto = _settings.AutoMode && !_autoSuspended;

        if (connected != null)
        {
            if (!_wasConnected) WriteLog($"«{connected.Name}» connesso.");
            _wasConnected = true;
            _disconnectedSince = null;
            _autoSuspended = false;
            Publish(LinkState.Connected, FlowStep.None, "Controller connesso", $"{connected.Name} · pronto all'uso");
            return;
        }

        if (_wasConnected)
        {
            WriteLog("Controller disconnesso.");
            _wasConnected = false;
        }

        if (paired.Count > 0)
        {
            _disconnectedSince ??= DateTime.UtcNow;
            int remaining = _settings.GraceSeconds - (int)(DateTime.UtcNow - _disconnectedSince.Value).TotalSeconds;
            if (auto && remaining <= 0)
            {
                WriteLog("Il controller non si è ricollegato: avvio la riparazione automatica.");
                _ = RunFlowAsync(isAuto: true);
                return;
            }
            Publish(LinkState.Disconnected, FlowStep.None, "Controller non connesso",
                auto
                    ? $"Abbinamento probabilmente bloccato. Riparazione automatica tra {Math.Max(remaining, 0)} s"
                    : "Premi «Connetti controller», poi il pulsante SYNC");
        }
        else
        {
            _disconnectedSince = null;
            if (auto)
            {
                _ = RunFlowAsync(isAuto: true);
                return;
            }
            Publish(LinkState.NotPaired, FlowStep.None, "Nessun controller abbinato",
                _autoSuspended ? "Ricerca automatica in pausa. Premi «Connetti controller»"
                               : "Premi «Connetti controller» per iniziare");
        }
    }

    // ───────────────────────────── Procedura di riabbinamento ─────────────────────────────

    async Task RunFlowAsync(bool isAuto)
    {
        if (!await _flowLock.WaitAsync(0)) return;
        var cts = CancellationTokenSource.CreateLinkedTokenSource(_loopCts.Token);
        _flowIsAuto = isAuto;
        _flowCts = cts;
        var ct = cts.Token;
        try
        {
            // 1. Rimozione dell'abbinamento esistente (quello che "si blocca")
            Publish(LinkState.Removing, FlowStep.Remove, "Rimozione abbinamento…", "Pulizia del vecchio collegamento", force: true);
            var paired = await GetPairedControllersAsync();
            foreach (var c in paired)
            {
                var r = await c.Info.Pairing.UnpairAsync();
                WriteLog($"Rimosso «{c.Name}»: {r.Status}");
            }
            if (paired.Count > 0) await Task.Delay(1500, ct);

            // 2. Attesa del controller in modalità SYNC
            Publish(LinkState.Searching, FlowStep.Sync,
                isAuto ? "In attesa del controller" : "Premi il pulsante SYNC",
                "Premi il piccolo tasto SYNC sul bordo superiore del controller finché i LED scorrono" +
                (isAuto ? ". Verrà abbinato in automatico." : ""), force: true);
            WriteLog(isAuto ? "Ricerca automatica avviata." : "Ricerca avviata (90 s).");

            var device = await DiscoverAsync(isAuto ? null : TimeSpan.FromSeconds(90), dutyCycle: isAuto, ct);
            if (device == null)
            {
                WriteLog("Nessun controller trovato.");
                Fail("Controller non trovato", "Assicurati che i LED scorrano dopo aver premuto SYNC e riprova");
                return;
            }
            WriteLog($"Trovato «{device.Name}».");

            // 3. Abbinamento
            Publish(LinkState.Pairing, FlowStep.Pair, "Abbinamento in corso…", device.Name, force: true);
            if (!await PairAsync(device, ct))
            {
                Fail("Abbinamento non riuscito", "Premi di nuovo SYNC e riprova");
                return;
            }

            // 4. Attesa della connessione
            Publish(LinkState.Pairing, FlowStep.Done, "Connessione…", "In attesa che il controller si colleghi", force: true);
            bool ok = await WaitConnectedAsync(device.Id, TimeSpan.FromSeconds(20), ct);
            _wasConnected = ok;
            if (ok)
            {
                WriteLog($"«{device.Name}» abbinato e connesso.");
                Publish(LinkState.Connected, FlowStep.Done, "Controller connesso", $"{device.Name} · pronto all'uso", force: true);
                Notify?.Invoke("Controller connesso", $"{device.Name} è pronto all'uso.");
            }
            else
            {
                WriteLog("Abbinato, ma la connessione non è ancora attiva.");
                Publish(LinkState.Disconnected, FlowStep.Pair, "Abbinato", "Premi un tasto del controller per collegarlo", force: true);
            }
            _holdUntil = DateTime.UtcNow.AddSeconds(4);
        }
        catch (OperationCanceledException)
        {
            WriteLog("Operazione annullata.");
            _holdUntil = DateTime.MinValue;
        }
        catch (Exception ex)
        {
            WriteLog("Errore: " + ex.Message);
            Fail("Si è verificato un errore", ex.Message);
        }
        finally
        {
            _flowCts = null;
            _flowIsAuto = false;
            _disconnectedSince = null;
            cts.Dispose();
            _flowLock.Release();
            Current = Current with { Busy = false, AutoFlow = false };
            StatusChanged?.Invoke(Current);
        }
    }

    void Fail(string title, string detail)
    {
        Publish(LinkState.Error, Current.Step, title, detail, force: true);
        _holdUntil = DateTime.UtcNow.AddSeconds(8);
    }

    // ───────────────────────────── Bluetooth (WinRT) ─────────────────────────────

    static async Task<Radio?> GetRadioAsync()
    {
        var adapter = await BluetoothAdapter.GetDefaultAsync();
        return adapter == null ? null : await adapter.GetRadioAsync();
    }

    public async Task<List<ControllerInfo>> GetPairedControllersAsync()
    {
        var selector = BluetoothDevice.GetDeviceSelectorFromPairingState(true);
        var infos = await DeviceInformation.FindAllAsync(selector, AepProps, DeviceInformationKind.AssociationEndpoint);
        var result = new List<ControllerInfo>();
        foreach (var d in infos)
        {
            if (!_settings.Matches(d.Name)) continue;
            bool connected = false;
            try
            {
                using var bt = await BluetoothDevice.FromIdAsync(d.Id);
                connected = bt?.ConnectionStatus == BluetoothConnectionStatus.Connected;
            }
            catch
            {
                if (d.Properties.TryGetValue("System.Devices.Aep.IsConnected", out var v) && v is bool b) connected = b;
            }
            result.Add(new ControllerInfo(d, d.Name, connected));
        }
        return result;
    }

    /// <summary>Cerca un controller non abbinato (cioè in modalità SYNC). timeout null = all'infinito.</summary>
    async Task<DeviceInformation?> DiscoverAsync(TimeSpan? timeout, bool dutyCycle, CancellationToken ct)
    {
        var deadline = timeout.HasValue ? DateTime.UtcNow + timeout.Value : DateTime.MaxValue;
        while (DateTime.UtcNow < deadline)
        {
            ct.ThrowIfCancellationRequested();
            var slice = TimeSpan.FromSeconds(20);
            if (timeout.HasValue && deadline - DateTime.UtcNow < slice) slice = deadline - DateTime.UtcNow;

            var found = await ScanOnceAsync(slice, ct);
            if (found != null) return found;

            // In automatico la ricerca è a cicli, per non occupare la radio senza sosta.
            if (dutyCycle) await Task.Delay(3000, ct);
        }
        return null;
    }

    async Task<DeviceInformation?> ScanOnceAsync(TimeSpan duration, CancellationToken ct)
    {
        var tcs = new TaskCompletionSource<DeviceInformation?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var seen = new Dictionary<string, DeviceInformation>();
        var watcher = DeviceInformation.CreateWatcher(
            BluetoothDevice.GetDeviceSelectorFromPairingState(false), AepProps, DeviceInformationKind.AssociationEndpoint);

        void Check(DeviceInformation d)
        {
            if (_settings.Matches(d.Name) && d.Pairing.CanPair) tcs.TrySetResult(d);
        }

        watcher.Added += (_, d) =>
        {
            lock (seen) seen[d.Id] = d;
            Check(d);
        };
        watcher.Updated += (_, u) =>
        {
            DeviceInformation? d;
            lock (seen)
            {
                if (!seen.TryGetValue(u.Id, out d)) return;
                d.Update(u);
            }
            Check(d);
        };
        watcher.Stopped += (_, _) => tcs.TrySetResult(null);

        watcher.Start();
        try
        {
            var winner = await Task.WhenAny(tcs.Task, Task.Delay(duration, ct));
            ct.ThrowIfCancellationRequested();
            return winner == tcs.Task ? tcs.Task.Result : null;
        }
        finally
        {
            try
            {
                if (watcher.Status is DeviceWatcherStatus.Started or DeviceWatcherStatus.EnumerationCompleted)
                    watcher.Stop();
            }
            catch { }
        }
    }

    async Task<bool> PairAsync(DeviceInformation device, CancellationToken ct)
    {
        const DevicePairingKinds kinds = DevicePairingKinds.ConfirmOnly | DevicePairingKinds.DisplayPin |
                                         DevicePairingKinds.ProvidePin | DevicePairingKinds.ConfirmPinMatch;

        TypedEventHandler<DeviceInformationCustomPairing, DevicePairingRequestedEventArgs> handler = (_, args) =>
        {
            if (args.PairingKind == DevicePairingKinds.ProvidePin) args.Accept("0000");
            else args.Accept();
        };

        for (int attempt = 1; attempt <= 3; attempt++)
        {
            ct.ThrowIfCancellationRequested();
            var custom = device.Pairing.Custom;
            custom.PairingRequested += handler;
            try
            {
                var result = await custom.PairAsync(kinds, DevicePairingProtectionLevel.None);
                WriteLog($"Abbinamento (tentativo {attempt}): {result.Status}");
                if (result.Status is DevicePairingResultStatus.Paired or DevicePairingResultStatus.AlreadyPaired)
                    return true;
            }
            finally
            {
                custom.PairingRequested -= handler;
            }
            await Task.Delay(1500, ct);
        }
        return false;
    }

    static async Task<bool> WaitConnectedAsync(string id, TimeSpan timeout, CancellationToken ct)
    {
        var end = DateTime.UtcNow + timeout;
        while (DateTime.UtcNow < end)
        {
            try
            {
                using var bt = await BluetoothDevice.FromIdAsync(id);
                if (bt?.ConnectionStatus == BluetoothConnectionStatus.Connected) return true;
            }
            catch { }
            await Task.Delay(700, ct);
        }
        return false;
    }

    // ───────────────────────────── Utility ─────────────────────────────

    void Publish(LinkState state, FlowStep step, string title, string detail, bool force = false)
    {
        if (!force && (FlowRunning || DateTime.UtcNow < _holdUntil)) return;
        var st = new ServiceStatus(state, step, title, detail, FlowRunning, _flowIsAuto);
        if (st == Current) return;
        Current = st;
        StatusChanged?.Invoke(st);
    }

    void WriteLog(string message)
    {
        Log?.Invoke(message);
        try
        {
            Directory.CreateDirectory(AppSettings.Folder);
            var file = Path.Combine(AppSettings.Folder, "log.txt");
            if (File.Exists(file) && new FileInfo(file).Length > 512 * 1024) File.Delete(file);
            File.AppendAllText(file, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss}  {message}{Environment.NewLine}");
        }
        catch { }
    }

    public void Dispose()
    {
        _loopCts.Cancel();
        _flowCts?.Cancel();
    }
}
